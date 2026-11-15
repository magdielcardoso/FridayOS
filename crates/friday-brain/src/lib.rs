//! Friday Brain: a Markdown-first knowledge graph indexed in SQLite + FTS5.
//!
//! Markdown stays the source of truth. The database is a rebuildable index of
//! notes, aliases, wiki links and typed relations.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

pub fn data_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/FridayOS")
}

pub fn notes_dir() -> PathBuf {
    data_dir().join("knowledge")
}

fn db_path() -> PathBuf {
    data_dir().join("knowledge.db")
}

fn memory_file() -> PathBuf {
    data_dir().join("memory.jsonl")
}

fn connection() -> anyhow::Result<Connection> {
    let _ = std::fs::create_dir_all(data_dir());
    let conn = Connection::open(db_path())?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS nodes (
            id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            title TEXT NOT NULL,
            path TEXT NOT NULL UNIQUE,
            updated INTEGER NOT NULL,
            body TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS edges (
            src TEXT NOT NULL,
            dst TEXT NOT NULL,
            rel TEXT NOT NULL,
            source TEXT NOT NULL,
            confidence REAL NOT NULL,
            explicit INTEGER NOT NULL,
            created INTEGER NOT NULL,
            PRIMARY KEY (src, dst, rel, source)
        );
        CREATE INDEX IF NOT EXISTS edges_src ON edges(src);
        CREATE INDEX IF NOT EXISTS edges_dst ON edges(dst);
        CREATE TABLE IF NOT EXISTS aliases (
            alias TEXT NOT NULL,
            node_id TEXT NOT NULL,
            PRIMARY KEY (alias, node_id)
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS nodes_fts USING fts5(id UNINDEXED, title, body);
        "#,
    )?;
    Ok(conn)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub path: String,
    pub body: String,
    pub updated: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub src: String,
    pub dst: String,
    pub rel: String,
    pub source: String,
    pub confidence: f64,
    pub explicit: bool,
}

#[derive(Debug, Default)]
struct Frontmatter {
    id: Option<String>,
    kind: Option<String>,
    aliases: Vec<String>,
    tags: Vec<String>,
}

fn parse_frontmatter(text: &str) -> (Frontmatter, &str) {
    if !text.starts_with("---") {
        return (Frontmatter::default(), text);
    }
    let rest = &text[3..];
    let Some(end) = rest.find("\n---") else {
        return (Frontmatter::default(), text);
    };
    let header = &rest[..end];
    let body = rest[end + 4..].trim_start_matches('\n');
    let mut meta = Frontmatter::default();
    let mut key = String::new();
    for line in header.lines() {
        if let Some(value) = line.strip_prefix("id:") {
            meta.id = Some(value.trim().to_string());
            key.clear();
        } else if let Some(value) = line.strip_prefix("type:") {
            meta.kind = Some(value.trim().to_string());
            key.clear();
        } else if let Some(value) = line.strip_prefix("aliases:") {
            let inline = value.trim();
            if !inline.is_empty() {
                meta.aliases.push(inline.trim_matches(['[', ']']).to_string());
            }
            key = "aliases".into();
        } else if let Some(value) = line.strip_prefix("tags:") {
            let inline = value.trim();
            if !inline.is_empty() {
                meta.tags.push(inline.trim_matches(['[', ']']).to_string());
            }
            key = "tags".into();
        } else if let Some(item) = line.trim().strip_prefix("- ") {
            match key.as_str() {
                "aliases" => meta.aliases.push(item.trim().to_string()),
                "tags" => meta.tags.push(item.trim().to_string()),
                _ => {}
            }
        } else if !line.trim().is_empty() {
            key.clear();
        }
    }
    (meta, body)
}

fn slug(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if ch.is_whitespace() || ch == '/' || ch == '_' || ch == '-' {
            if !out.ends_with('-') {
                out.push('-');
            }
        }
    }
    out.trim_matches('-').to_string()
}

/// Extracts `[[target]]`, `[[target|alias]]`, `[[folder/target]]`, `[[target#section]]`.
pub fn wiki_links(body: &str) -> Vec<String> {
    let mut links = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else { break };
        let inner = &after[..end];
        let target = inner
            .split('|')
            .next()
            .unwrap_or("")
            .split('#')
            .next()
            .unwrap_or("")
            .trim();
        if !target.is_empty() {
            links.push(target.to_string());
        }
        rest = &after[end + 2..];
    }
    links
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn upsert(conn: &Connection, node: &Node, aliases: &[String], links: &[String]) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO nodes(id, kind, title, path, updated, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(path) DO UPDATE SET kind=excluded.kind, title=excluded.title,
         updated=excluded.updated, body=excluded.body",
        params![node.id, node.kind, node.title, node.path, node.updated, node.body],
    )?;
    conn.execute("DELETE FROM nodes_fts WHERE id = ?1", params![node.id])?;
    conn.execute(
        "INSERT INTO nodes_fts(id, title, body) VALUES (?1, ?2, ?3)",
        params![node.id, node.title, node.body],
    )?;
    conn.execute("DELETE FROM aliases WHERE node_id = ?1", params![node.id])?;
    for alias in aliases {
        conn.execute(
            "INSERT OR IGNORE INTO aliases(alias, node_id) VALUES (?1, ?2)",
            params![alias.to_lowercase(), node.id],
        )?;
    }
    conn.execute(
        "DELETE FROM edges WHERE src = ?1 AND source = 'markdown'",
        params![node.id],
    )?;
    for link in links {
        let dst = slug(link);
        conn.execute(
            "INSERT OR IGNORE INTO edges(src, dst, rel, source, confidence, explicit, created)
             VALUES (?1, ?2, 'related_to', 'markdown', 1.0, 1, ?3)",
            params![node.id, dst, now()],
        )?;
    }
    Ok(())
}

fn index_markdown(conn: &Connection) -> anyhow::Result<usize> {
    let dir = notes_dir();
    let _ = std::fs::create_dir_all(&dir);
    let mut count = 0;
    for entry in walk(&dir) {
        let Ok(text) = std::fs::read_to_string(&entry) else {
            continue;
        };
        let (meta, body) = parse_frontmatter(&text);
        let relative = entry
            .strip_prefix(&dir)
            .unwrap_or(&entry)
            .with_extension("");
        let id = meta.id.unwrap_or_else(|| slug(&relative.to_string_lossy()));
        let title = relative
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| id.clone());
        let node = Node {
            id: id.clone(),
            kind: meta.kind.unwrap_or_else(|| "note".into()),
            title,
            path: entry.to_string_lossy().to_string(),
            body: body.to_string(),
            updated: now(),
        };
        let links = wiki_links(body);
        upsert(conn, &node, &meta.aliases, &links)?;
        for tag in meta.tags {
            conn.execute(
                "INSERT OR IGNORE INTO aliases(alias, node_id) VALUES (?1, ?2)",
                params![format!("tag:{tag}"), id],
            )?;
        }
        count += 1;
    }
    Ok(count)
}

fn index_memories(conn: &Connection) -> anyhow::Result<usize> {
    let Ok(text) = std::fs::read_to_string(memory_file()) else {
        return Ok(0);
    };
    let mut count = 0;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let content = value.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let id = value
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("memory")
            .to_string();
        if content.is_empty() {
            continue;
        }
        let node = Node {
            id: format!("memory-{id}"),
            kind: "memory".into(),
            title: content.chars().take(60).collect(),
            path: format!("memory.jsonl#{id}"),
            body: content.to_string(),
            updated: value.get("ts").and_then(|v| v.as_u64()).unwrap_or(0) as i64,
        };
        let links = wiki_links(content);
        upsert(conn, &node, &[], &links)?;
        count += 1;
    }
    Ok(count)
}

/// Deterministic, cheap relation inference: nodes that share meaningful words
/// get an inferred `related_to` edge. No LLM, no embeddings.
fn infer_edges(conn: &Connection) -> anyhow::Result<usize> {
    let mut statement = conn.prepare("SELECT id, title, body FROM nodes")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut items: Vec<(String, String, String)> = rows.filter_map(|row| row.ok()).collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));

    let tokens = |text: &str| -> std::collections::HashSet<String> {
        const STOP: [&str; 24] = [
            "para", "pelo", "pela", "como", "mais", "menos", "this", "that", "with", "have",
            "from", "user", "você", "voce", "senhor", "friday", "deve", "deveria", "pode",
            "quando", "onde", "sobre", "isso", "esta",
        ];
        text.to_lowercase()
            .split(|ch: char| !ch.is_alphanumeric() && ch != '-')
            .filter(|word| word.len() >= 4)
            .filter(|word| !STOP.contains(word))
            .map(str::to_string)
            .collect()
    };

    let sets: Vec<std::collections::HashSet<String>> = items
        .iter()
        .map(|(_, title, body)| tokens(&format!("{title} {body}")))
        .collect();

    let mut created = 0;
    for a in 0..items.len() {
        let mut candidates: Vec<(usize, f32, usize)> = Vec::new();
        for b in (a + 1)..items.len() {
            if sets[a].is_empty() || sets[b].is_empty() {
                continue;
            }
            let common = sets[a].intersection(&sets[b]).count();
            if common < 2 {
                continue;
            }
            let score = common as f32
                / ((sets[a].len() as f32).sqrt() * (sets[b].len() as f32).sqrt()).max(1.0);
            if score >= 0.16 {
                candidates.push((b, score, common));
            }
        }
        candidates.sort_by(|left, right| right.1.partial_cmp(&left.1).unwrap_or(std::cmp::Ordering::Equal));
        for (b, score, _) in candidates.into_iter().take(4) {
            let inserted = conn.execute(
                "INSERT OR IGNORE INTO edges(src, dst, rel, source, confidence, explicit, created)
                 VALUES (?1, ?2, 'related_to', 'inferred', ?3, 0, ?4)",
                params![items[a].0, items[b].0, score as f64, now()],
            )?;
            created += inserted;
            let inserted = conn.execute(
                "INSERT OR IGNORE INTO edges(src, dst, rel, source, confidence, explicit, created)
                 VALUES (?2, ?1, 'related_to', 'inferred', ?3, 0, ?4)",
                params![items[a].0, items[b].0, score as f64, now()],
            )?;
            created += inserted;
        }
    }
    Ok(created)
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            files.push(path);
        }
    }
    files
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexReport {
    pub notes: usize,
    pub memories: usize,
    pub db: String,
}

/// Rebuilds the whole index from Markdown + memories. Cheap for local vaults.
pub fn index_all() -> anyhow::Result<IndexReport> {
    let conn = connection()?;
    let notes = index_markdown(&conn)?;
    let memories = index_memories(&conn)?;
    let _ = infer_edges(&conn);
    Ok(IndexReport {
        notes,
        memories,
        db: db_path().display().to_string(),
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub snippet: String,
}

fn fts_query(query: &str) -> String {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|term| term.trim_matches(|ch: char| !ch.is_alphanumeric()))
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{term}\"*"))
        .collect();
    if terms.is_empty() {
        "friday".into()
    } else {
        terms.join(" OR ")
    }
}

pub fn search(query: &str, limit: usize) -> anyhow::Result<Vec<SearchHit>> {
    let conn = connection()?;
    let mut statement = conn.prepare(
        "SELECT n.id, n.title, n.kind, snippet(nodes_fts, 2, '', '', '…', 18)
         FROM nodes_fts JOIN nodes n ON n.id = nodes_fts.id
         WHERE nodes_fts MATCH ?1 ORDER BY rank LIMIT ?2",
    )?;
    let rows = statement.query_map(params![fts_query(query), limit as i64], |row| {
        Ok(SearchHit {
            id: row.get(0)?,
            title: row.get(1)?,
            kind: row.get(2)?,
            snippet: row.get::<_, String>(3).unwrap_or_default(),
        })
    })?;
    Ok(rows.filter_map(|row| row.ok()).collect())
}

/// Node ids matching an exact alias (e.g. a person or project name).
pub fn resolve(name: &str) -> anyhow::Result<Vec<String>> {
    let conn = connection()?;
    let mut statement = conn.prepare("SELECT node_id FROM aliases WHERE alias = ?1")?;
    let rows = statement.query_map(params![name.to_lowercase()], |row| row.get(0))?;
    let mut ids: Vec<String> = rows.filter_map(|row| row.ok()).collect();
    if ids.is_empty() {
        ids = search(name, 5)?.into_iter().map(|hit| hit.id).collect();
    }
    Ok(ids)
}

#[derive(Debug, Clone, Serialize)]
pub struct Neighbors {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

pub fn neighbors(id: &str, depth: usize) -> anyhow::Result<Neighbors> {
    let conn = connection()?;
    let depth = depth.clamp(1, 3);
    let mut ids = vec![id.to_string()];
    let mut frontier = vec![id.to_string()];
    let mut edges: Vec<Edge> = Vec::new();
    for _ in 0..depth {
        let mut next = Vec::new();
        for current in frontier.drain(..) {
            for edge in edges_of(&conn, &current)? {
                if !ids.contains(&edge.dst) {
                    ids.push(edge.dst.clone());
                    next.push(edge.dst.clone());
                }
                if !ids.contains(&edge.src) {
                    ids.push(edge.src.clone());
                    next.push(edge.src.clone());
                }
                if !edges.iter().any(|e| {
                    e.src == edge.src && e.dst == edge.dst && e.rel == edge.rel
                }) {
                    edges.push(edge);
                }
            }
        }
        frontier = next;
    }
    let nodes = ids
        .iter()
        .filter_map(|id| node_by_id(&conn, id).ok().flatten())
        .collect();
    Ok(Neighbors { nodes, edges })
}

fn edges_of(conn: &Connection, id: &str) -> anyhow::Result<Vec<Edge>> {
    let mut statement = conn.prepare(
        "SELECT src, dst, rel, source, confidence, explicit FROM edges WHERE src = ?1 OR dst = ?1",
    )?;
    let rows = statement.query_map(params![id], |row| {
        Ok(Edge {
            src: row.get(0)?,
            dst: row.get(1)?,
            rel: row.get(2)?,
            source: row.get(3)?,
            confidence: row.get(4)?,
            explicit: row.get::<_, i64>(5)? != 0,
        })
    })?;
    Ok(rows.filter_map(|row| row.ok()).collect())
}

pub fn node_by_id(conn: &Connection, id: &str) -> anyhow::Result<Option<Node>> {
    let mut statement = conn.prepare(
        "SELECT id, kind, title, path, body, updated FROM nodes WHERE id = ?1 LIMIT 1",
    )?;
    let mut rows = statement.query(params![id])?;
    if let Some(row) = rows.next()? {
        Ok(Some(Node {
            id: row.get(0)?,
            kind: row.get(1)?,
            title: row.get(2)?,
            path: row.get(3)?,
            body: row.get(4)?,
            updated: row.get(5)?,
        }))
    } else {
        Ok(None)
    }
}

pub fn all_nodes() -> anyhow::Result<Vec<Node>> {
    let conn = connection()?;
    let mut statement = conn.prepare("SELECT id, kind, title, path, body, updated FROM nodes")?;
    let rows = statement.query_map([], |row| {
        Ok(Node {
            id: row.get(0)?,
            kind: row.get(1)?,
            title: row.get(2)?,
            path: row.get(3)?,
            body: row.get(4)?,
            updated: row.get(5)?,
        })
    })?;
    Ok(rows.filter_map(|row| row.ok()).collect())
}

pub fn all_edges() -> anyhow::Result<Vec<Edge>> {
    let conn = connection()?;
    let mut statement =
        conn.prepare("SELECT src, dst, rel, source, confidence, explicit FROM edges")?;
    let rows = statement.query_map([], |row| {
        Ok(Edge {
            src: row.get(0)?,
            dst: row.get(1)?,
            rel: row.get(2)?,
            source: row.get(3)?,
            confidence: row.get(4)?,
            explicit: row.get::<_, i64>(5)? != 0,
        })
    })?;
    Ok(rows.filter_map(|row| row.ok()).collect())
}

/// Builds a self-contained interactive graph page and opens it in the browser.
pub fn open_panel(highlight: &[String]) -> anyhow::Result<PathBuf> {
    let nodes = all_nodes()?;
    let edges = all_edges()?;
    let payload = serde_json::json!({
        "nodes": nodes.iter().map(|node| serde_json::json!({
            "id": node.id, "title": node.title, "kind": node.kind, "body": node.body
        })).collect::<Vec<_>>(),
        "edges": edges,
        "highlight": highlight,
    });
    let html = panel_html(&payload.to_string());
    let path = std::env::temp_dir().join("friday-knowledge-graph.html");
    std::fs::write(&path, html)?;
    let _ = std::process::Command::new("/usr/bin/open")
        .arg(&path)
        .status();
    Ok(path)
}

fn panel_html(payload: &str) -> String {
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>Friday · Knowledge</title>
<style>
body{{margin:0;background:#050505;color:#ff2f2f;font-family:Menlo,monospace;overflow:hidden}}
header{{padding:12px 16px;border-bottom:1px solid #3b0d0d;display:flex;gap:16px;align-items:center;font-size:12px}}
#search{{background:#0b0303;border:1px solid #3b0d0d;color:#e8dcdc;padding:6px 10px;font:inherit;border-radius:4px;width:260px}}
canvas{{display:block;width:100vw;height:calc(100vh - 49px)}}
#tip{{position:fixed;pointer-events:none;background:#0d0303;border:1px solid #8a1c1c;padding:6px 8px;font-size:11px;color:#e8dcdc;display:none;max-width:280px}}
</style></head><body>
<header><b>F.R.I.D.A.Y · GRAFO</b><input id="search" placeholder="buscar nó…"><span id="count"></span></header>
<canvas id="c"></canvas><div id="tip"></div>
<script>
const data = {payload};
const canvas = document.getElementById('c'), ctx = canvas.getContext('2d');
const hl = new Set(data.highlight || []);
let nodes = data.nodes.map((n,i)=>({{...n,
  x: innerWidth/2 + Math.cos(i*2.399)*Math.min(innerWidth,innerHeight)*0.35,
  y: innerHeight/2 + Math.sin(i*2.399)*Math.min(innerWidth,innerHeight)*0.35,
  vx:0, vy:0}}));
const byId = new Map(nodes.map(n=>[n.id,n]));
const edges = data.edges.filter(e=>byId.has(e.src)&&byId.has(e.dst));
document.getElementById('count').textContent = nodes.length+' nós · '+edges.length+' relações';
let scale=1, ox=0, oy=0, drag=null, pan=null, hover=null;
function resize(){{canvas.width=innerWidth;canvas.height=innerHeight-49;}}
addEventListener('resize',resize);resize();
function toWorld(x,y){{return [ (x-ox)/scale, (y-oy)/scale ];}}
function draw(){{
  ctx.setTransform(1,0,0,1,0,0);ctx.clearRect(0,0,canvas.width,canvas.height);
  ctx.setTransform(scale,0,0,scale,ox,oy);
  const focus = hl.size>0;
  for(const e of edges){{
    const a=byId.get(e.src), b=byId.get(e.dst);
    const hot = hl.has(e.src)&&hl.has(e.dst);
    ctx.strokeStyle = hot ? '#ff2f2f' : (focus?'rgba(59,13,13,.35)':'rgba(138,28,28,.35)');
    ctx.lineWidth = hot ? 2.4 : 1;
    ctx.beginPath();ctx.moveTo(a.x,a.y);ctx.lineTo(b.x,b.y);ctx.stroke();
  }}
  for(const n of nodes){{
    const hot = hl.has(n.id);
    const dim = focus && !hot;
    ctx.beginPath();ctx.arc(n.x,n.y,hot?7:4.5,0,7);
    ctx.fillStyle = hot ? '#ff2f2f' : (dim?'rgba(138,28,28,.45)':'#8a1c1c');
    ctx.fill();
    if(n===hover||hot||nodes.length<80){{
      ctx.fillStyle = hot ? '#e8dcdc' : (dim?'rgba(232,220,220,.35)':'#c9a9a9');
      ctx.font='11px Menlo';ctx.fillText(n.title.slice(0,26), n.x+9, n.y+4);
    }}
  }}
}}
function tick(){{
  for(const n of nodes){{n.vx*=0.9;n.vy*=0.9;n.x+=n.vx;n.y+=n.vy;}}
  draw();requestAnimationFrame(tick);
}}
canvas.onmousedown=e=>{{
  const [x,y]=toWorld(e.offsetX,e.offsetY);
  drag = nodes.find(n=>Math.hypot(n.x-x,n.y-y)<12/scale) || null;
  if(!drag) pan={{x:e.offsetX,y:e.offsetY,ox,oy}};
}};
canvas.onmousemove=e=>{{
  const [x,y]=toWorld(e.offsetX,e.offsetY);
  hover = nodes.find(n=>Math.hypot(n.x-x,n.y-y)<12/scale) || null;
  const tip=document.getElementById('tip');
  if(hover){{tip.style.display='block';tip.style.left=(e.clientX+12)+'px';tip.style.top=(e.clientY+12)+'px';
    tip.innerHTML='<b>'+hover.title+'</b><br>'+(hover.body||'').slice(0,240);}}
  else tip.style.display='none';
  if(drag){{drag.x=x;drag.y=y;}}
  else if(pan){{ox=pan.ox+(e.offsetX-pan.x);oy=pan.oy+(e.offsetY-pan.y);}}
}};
addEventListener('mouseup',()=>{{drag=null;pan=null;}});
canvas.onwheel=e=>{{e.preventDefault();const f=e.deltaY<0?1.1:0.9;
  const mx=e.offsetX,my=e.offsetY;ox=mx-(mx-ox)*f;oy=my-(my-oy)*f;scale*=f;}};
document.getElementById('search').oninput=e=>{{
  const q=e.target.value.toLowerCase();
  hl.clear(); if(q) for(const n of nodes) if((n.title+' '+n.body).toLowerCase().includes(q)) hl.add(n.id);
}};
tick();
</script></body></html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wiki_links() {
        let links = wiki_links("veja [[Projeto Friday|Friday]] e [[pasta/nota#Seção]]");
        assert_eq!(links, vec!["Projeto Friday", "pasta/nota"]);
    }

    #[test]
    fn parses_frontmatter() {
        let text = "---\nid: project-friday\ntype: project\naliases:\n  - Friday\n---\ncorpo [[X]]";
        let (meta, body) = parse_frontmatter(text);
        assert_eq!(meta.id.as_deref(), Some("project-friday"));
        assert_eq!(meta.kind.as_deref(), Some("project"));
        assert_eq!(meta.aliases, vec!["Friday"]);
        assert!(body.contains("[[X]]"));
    }

    #[test]
    fn slugifies_titles() {
        assert_eq!(slug("Projeto Friday / v2"), "projeto-friday-v2");
    }
}
