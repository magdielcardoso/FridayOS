const chat = document.getElementById('chat');
const input = document.getElementById('input');
const sendBtn = document.getElementById('send');
const micBtn = document.getElementById('mic');
const dot = document.getElementById('dot');
const statusEl = document.getElementById('status');
const pair = document.getElementById('pair');
const toastEl = document.getElementById('toast');

let ws = null;
let current = null;        // assistant bubble being streamed
let reconnectTimer = null;

function toast(text) {
  toastEl.textContent = text;
  toastEl.classList.add('show');
  setTimeout(() => toastEl.classList.remove('show'), 2200);
}

function escapeHtml(s) {
  return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
}
function renderMarkdown(text) {
  let html = escapeHtml(text);
  html = html.replace(/```([\s\S]*?)```/g, (_, code) => `<pre>${code}</pre>`);
  html = html.replace(/`([^`]+)`/g, '<code>$1</code>');
  html = html.replace(/\*\*([^*]+)\*\*/g, '<b>$1</b>');
  html = html.replace(/(https?:\/\/[^\s]+)/g, '<a href="$1" target="_blank" rel="noreferrer">$1</a>');
  return html;
}

function bubble(role, text) {
  const el = document.createElement('div');
  el.className = `msg ${role}`;
  el.innerHTML = renderMarkdown(text || '');
  chat.appendChild(el);
  chat.scrollTop = chat.scrollHeight;
  return el;
}

function setConn(on) {
  dot.classList.toggle('on', on);
  statusEl.textContent = on ? 'online' : 'offline';
}

async function api(path, options = {}) {
  const response = await fetch(path, {
    ...options,
    credentials: 'same-origin',
    headers: { 'X-Friday-Request': '1', ...(options.headers || {}) },
  });
  if (response.status === 401) {
    pair.classList.remove('hidden');
    throw new Error('unauthorized');
  }
  return response;
}

async function checkSession() {
  let response = await fetch('/api/session', { credentials: 'same-origin' });
  if (response.status === 401) {
    // Cookie lost or expired: reuse the pairing saved in localStorage.
    if (await autoPair()) {
      response = await fetch('/api/session', { credentials: 'same-origin' });
    }
  }
  if (response.status === 401) {
    pair.classList.remove('hidden');
    document.getElementById('code').value = storedPairCode();
    return false;
  }
  const data = await response.json();
  chat.innerHTML = '';
  for (const item of data.messages || []) bubble(item.role === 'user' ? 'user' : 'assistant', item.text);
  return true;
}

async function pairWith(code, silent = false) {
  const response = await fetch('/api/pair', {
    method: 'POST',
    credentials: 'same-origin',
    headers: { 'Content-Type': 'application/json', 'X-Friday-Request': '1' },
    body: JSON.stringify({ code }),
  });
  if (!response.ok) {
    localStorage.removeItem('friday_pair_code');
    if (!silent) toast('código inválido');
    return false;
  }
  localStorage.setItem('friday_pair_code', code);
  pair.classList.add('hidden');
  if (!silent) toast('pareado');
  connect();
  return true;
}

function pairDevice() {
  const code = document.getElementById('code').value.trim();
  return pairWith(code);
}

function storedPairCode() {
  try { return localStorage.getItem('friday_pair_code') || ''; } catch (e) { return ''; }
}

async function autoPair() {
  const code = storedPairCode();
  if (!code) return false;
  return pairWith(code, true);
}

function connect() {
  if (ws && ws.readyState <= 1) return;
  const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
  ws = new WebSocket(`${scheme}://${location.host}/api/chat/ws`);
  ws.onopen = () => { setConn(true); if (reconnectTimer) clearTimeout(reconnectTimer); };
  ws.onclose = () => {
    setConn(false);
    reconnectTimer = setTimeout(connect, 2500);
  };
  ws.onmessage = event => {
    const data = JSON.parse(event.data);
    if (data.type === 'history') {
      chat.innerHTML = '';
      for (const item of data.messages || []) bubble(item.role === 'user' ? 'user' : 'assistant', item.text);
      current = null;
    } else if (data.type === 'user') {
      bubble('user', data.text);
      current = null;
    } else if (data.type === 'delta') {
      if (!current) current = bubble('assistant', '');
      current.dataset.text = (current.dataset.text || '') + data.text;
      current.innerHTML = renderMarkdown(current.dataset.text);
      chat.scrollTop = chat.scrollHeight;
    } else if (data.type === 'done') {
      if (current) {
        current.dataset.text = data.text || current.dataset.text;
        current.innerHTML = renderMarkdown(current.dataset.text || '');
      }
      current = null;
    } else if (data.type === 'error') {
      bubble('assistant', `⚠ ${data.text}`);
      current = null;
    }
  };
}

async function send() {
  const text = input.value.trim();
  if (!text) return;
  input.value = '';
  input.style.height = 'auto';
  try {
    const response = await api('/api/chat', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ text }),
    });
    if (!response.ok) toast('falha ao enviar');
  } catch (error) {
    if (error.message !== 'unauthorized') toast('sem conexão');
  }
}

sendBtn.onclick = send;
input.addEventListener('keydown', event => {
  if (event.key === 'Enter' && !event.shiftKey) { event.preventDefault(); send(); }
});
input.addEventListener('input', () => {
  input.style.height = 'auto';
  input.style.height = Math.min(input.scrollHeight, 120) + 'px';
});
document.getElementById('pairBtn').onclick = pairDevice;

// ---- voice ----
async function encodeWav(blob) {
  const buffer = await blob.arrayBuffer();
  const ctx = new OfflineAudioContext(1, 1, 16000);
  const decoded = await new AudioContext().decodeAudioData(buffer);
  const frames = Math.ceil(decoded.duration * 16000);
  const render = new OfflineAudioContext(1, frames, 16000);
  const source = render.createBufferSource();
  source.buffer = decoded;
  source.connect(render.destination);
  source.start();
  const rendered = await render.startRendering();
  const samples = rendered.getChannelData(0);
  const pcm = new Int16Array(samples.length);
  for (let i = 0; i < samples.length; i++) pcm[i] = Math.max(-1, Math.min(1, samples[i])) * 32767;
  const wav = new ArrayBuffer(44 + pcm.length * 2);
  const view = new DataView(wav);
  const writeStr = (offset, s) => { for (let i = 0; i < s.length; i++) view.setUint8(offset + i, s.charCodeAt(i)); };
  writeStr(0, 'RIFF'); view.setUint32(4, 36 + pcm.length * 2, true); writeStr(8, 'WAVE');
  writeStr(12, 'fmt '); view.setUint32(16, 16, true); view.setUint16(20, 1, true); view.setUint16(22, 1, true);
  view.setUint32(24, 16000, true); view.setUint32(28, 32000, true); view.setUint16(32, 2, true); view.setUint16(34, 16, true);
  writeStr(36, 'data'); view.setUint32(40, pcm.length * 2, true);
  new Int16Array(wav, 44).set(pcm);
  return new Blob([wav], { type: 'audio/wav' });
}

let recorder = null;
micBtn.onclick = async () => {
  if (recorder) { recorder.stop(); return; }
  if (!window.isSecureContext || !navigator.mediaDevices) {
    toast('voz exige HTTPS: habilite o Tailscale Serve e abra pela URL https');
    return;
  }
  try {
    const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    recorder = new MediaRecorder(stream);
    const chunks = [];
    recorder.ondataavailable = e => chunks.push(e.data);
    recorder.onstop = async () => {
      stream.getTracks().forEach(t => t.stop());
      recorder = null;
      micBtn.classList.remove('rec');
      const blob = new Blob(chunks, { type: 'audio/webm' });
      toast('transcrevendo…');
      try {
        const wav = await encodeWav(blob);
        const response = await api('/api/voice/transcribe', { method: 'POST', body: wav, headers: { 'Content-Type': 'audio/wav' } });
        const data = await response.json();
        if (data.text) { input.value = data.text; send(); }
        else toast('não entendi');
      } catch (error) { toast('falha na transcrição'); }
    };
    recorder.start();
    micBtn.classList.add('rec');
    toast('gravando… toque para parar');
  } catch (error) {
    const denied = String(error && error.name).includes('NotAllowed');
    toast(denied ? 'permissão de microfone negada' : 'microfone indisponível');
  }
};

// ---- push ----
async function enablePush() {
  if (!('serviceWorker' in navigator) || !('PushManager' in window)) { toast('push indisponível'); return; }
  const registration = await navigator.serviceWorker.ready;
  const permission = await Notification.requestPermission();
  if (permission !== 'granted') { toast('permissão negada'); return; }
  const keyResponse = await api('/api/push/public-key');
  const { key } = await keyResponse.json();
  const padding = '='.repeat((4 - key.length % 4) % 4);
  const raw = atob((key + padding).replace(/-/g, '+').replace(/_/g, '/'));
  const bytes = Uint8Array.from([...raw].map(c => c.charCodeAt(0)));
  const subscription = await registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: bytes });
  await api('/api/push/subscribe', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(subscription) });
  toast('notificações ativadas');
  await api('/api/push/test', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: '{}' });
}

async function boot() {
  if ('serviceWorker' in navigator) {
    try { await navigator.serviceWorker.register('/sw.js'); } catch (e) {}
  }
  const ok = await checkSession().catch(() => false);
  if (ok) {
    pair.classList.add('hidden');
    connect();
  } else {
    document.getElementById('code').value = storedPairCode();
  }
  document.getElementById('pushBtn').onclick = enablePush;
  // Long press the brand also enables push.
  let timer = null;
  document.querySelector('.brand').addEventListener('touchstart', () => { timer = setTimeout(enablePush, 700); });
  document.querySelector('.brand').addEventListener('touchend', () => clearTimeout(timer));
  document.querySelector('.brand').addEventListener('mousedown', () => { timer = setTimeout(enablePush, 700); });
  document.querySelector('.brand').addEventListener('mouseup', () => clearTimeout(timer));
}

boot();
