# Permissoes do macOS (TCC)

A Friday so enxerga e age no que o macOS permitir. TCC (Transparency, Consent
and Control) e por **app responsavel**, nao por usuario. Em desenvolvimento, o
app responsavel costuma ser o Terminal, o iTerm ou o Codex que iniciou o
processo; no produto final e o proprio `Friday.app`. Conceder a permissao ao
Terminal nao a concede ao app empacotado, e vice-versa.

Verifique o estado atual com:

```sh
./scripts/permissions.sh
```

O script so le e testa; nao altera nenhuma permissao.

## Matriz

| Permissao (System Settings) | Para que a Friday usa | Ferramentas afetadas | Sem ela |
|---|---|---|---|
| Acessibilidade | Ler a arvore AX e sintetizar teclado/mouse | `ax_*`, `input_*`, `input_trusted`, `shortcut_run` | AX falha; cliques e digitacao nao acontecem; `input_trusted` retorna falso |
| Gravacao de Tela e Audio do Sistema | Capturar a tela para OCR | `screen_capture`, `screen_capture_window`, `screen_read_text`, `screen_find_text` | Captura vazia ou erro; OCR sem texto |
| Microfone | Capturar a voz do usuario | pipeline de voz, `voice_devices` (lista ok, captura nao) | Nao ouve; VAD nunca dispara |
| Reconhecimento de Fala | Transcrever audio local (quando usando Speech) | `voice_listen_once`, pipeline STT local | Transcricao local falha; STT remoto continua possivel |
| Acesso Total ao Disco | Ler arquivos protegidos do usuario | `fs_*`, backups | `~/Library/Mail`, `~/Library/Safari`, Mensagens e afins negam acesso |
| Automacao (por app alvo) | Controlar Finder, System Events, Music etc. | `app_*`, `shortcut_run`, scripts `osascript` | Erro `-1743` ("not authorized to send Apple events") |
| Contatos | Buscar pessoas | `contacts_search` | Lista vazia ou erro de autorizacao |
| Calendarios | Ler e criar eventos | `calendar_today`, `calendar_create` | Sem acesso ao calendario padrao |
| Lembretes | Ler e criar lembretes | `reminders_list`, `reminder_add` | Sem acesso a lista padrao |

## Como conceder

Abra **Ajustes do Sistema > Privacidade e Seguranca** e entre na secao
correspondente:

1. **Acessibilidade** e **Gravacao de Tela e Audio do Sistema**: clique em
   `+`, escolha o app (ou o Terminal, em dev) e ative o interruptor. Em geral
   e preciso reiniciar o app depois de conceder Gravacao de Tela.
2. **Microfone**, **Reconhecimento de Fala**, **Contatos**, **Calendarios**,
   **Lembretes**: a primeira chamada abre o pedido do sistema; responda
   Permitir. Se negou antes, va na lista e ative.
3. **Acesso Total ao Disco**: `+`, adicione o app e ative. Feche e reabra o
   processo. Nao conceda por conveniencia: prefira apontar as ferramentas para
   pastas dentro de `~/Documents` quando possivel.
4. **Automacao**: a primeira vez que a Friday manda um Apple Event, o macOS
   pergunta "X quer controlar Y". Autorize por par (app que controla, app
   controlado). Para varios alvos, repita. Recusar nao quebra as outras
   ferramentas, so aquele alvo.

Dica de desenvolvimento: rode `./scripts/permissions.sh` antes e depois de
mexer nos Ajustes para ver o que mudou. Ao trocar de binario (por exemplo,
`cargo run` vs `target/debug/friday-app`), o macOS pode tratar como outro app
e pedir de novo.

## Como verificar

### Acessibilidade

```sh
osascript -e 'tell application "System Events" to get name of first process'
```

Se imprimir um nome, o processo que rodou o comando tem Acessibilidade. Se der
erro `-1719` ou "not allowed assistive access", falta.

Equivalente em codigo:

```sh
# dentro do app, a ferramenta input_trusted responde:
cargo run -p friday-tools --bin friday-tools   # confirma que a ferramenta existe
```

`input_trusted` retorna `{"trusted":true|false,"prompted":...}`.

### Gravacao de Tela

```sh
tmp="$(mktemp -t friday-tcc).png"
screencapture -x "$tmp" && ls -l "$tmp"
rm -f "$tmp"
```

Sem permissao, o comando pode sair com sucesso mas gerar uma imagem sem as
janelas (so o papel de parede) ou falhar. Compare o tamanho e o conteudo com o
esperado; o script faz a checagem basica de existencia e tamanho.

### Microfone

```sh
system_profiler SPAudioDataType | sed -n '/Input/,/Output/p'
```

Lista dispositivos de entrada. Isso prova que existe hardware e driver; a
permissao de captura so e testada ao abrir um stream (o pipeline de voz faz
isso e reporta erro de TCC).

### Reconhecimento de Fala

Ainda nao ha helper fixo. Quando existir, ele fica em
`~/Library/Caches/FridayOS/bin/` (ver `crates/friday-tools/src/swift.rs`) e o
script `permissions.sh` o executa se presente. Sem helper, o item aparece como
`--` (nao testado), nunca como falha.

### Acesso Total ao Disco

```sh
ls ~/Library/Mail >/dev/null 2>&1 && echo OK || echo FALTA
```

Se `~/Library/Mail` nao existir (Mail nunca usado), o teste e inconclusivo.
Alternativa: `ls ~/Library/Safari` ou ler um arquivo em
`~/Library/Application Support/com.apple.TCC`. Sem FDA, o erro tipico e
"Operation not permitted".

### Automacao

```sh
osascript -e 'tell application "Finder" to get name'
```

Finder esta sempre rodando; se retornar `Finder`, ha Automacao para Finder. O
erro `-1743` significa negado. Para testar System Events, use o teste de
Acessibilidade acima (ele tambem envolve Automacao com System Events).

### Contatos, Calendarios, Lembretes

Nao ha CLI estavel e sem efeito colateral para testar. A verificacao confiavel
e a propria ferramenta (`contacts_search`, `calendar_today`,
`reminders_list`) rodando com `dry_run=false` e devolvendo dados. Use
`dry_run=true` para validar o schema sem tocar no banco.

## O que quebra sem cada permissao

- **Sem Acessibilidade:** a Friday vira somente leitura de arquivos/rede. Nao
  clica, nao digita, nao le janelas de apps. `ax_*` e `input_*` retornam erro.
- **Sem Gravacao de Tela:** `screen_read_text` nao funciona; `screen_capture` pode
  devolver imagem inutil. A Friday nao "ve" a tela.
- **Sem Microfone:** o modo de voz fica mudo. O resto do app continua.
- **Sem Fala:** STT local nao roda; se o pipeline remoto estiver disponivel,
  a voz continua, mas com latencia e custo.
- **Sem FDA:** arquivos do usuario fora das pastas publicas falham. Nada de
  Mail, Mensagens, Safari, backups. O erro e por caminho, nao global.
- **Sem Automacao para um app:** so aquele app fica fora de alcance. Erros
  `-1743` devem ser traduzidos para "autorize a Friday em Automacao".
- **Sem Contatos/Calendarios/Lembretes:** as ferramentas correspondentes
  negam; o resto do catalogo segue normal.

## Notas de seguranca

- Conceda o minimo. Acessibilidade + Gravacao de Tela ja dao controle e visao
  amplos; trate-as como privilegio alto.
- Nao conceda FDA "para testar". Prefira caminhos explicitos.
- Revogar e instantaneo nos Ajustes. Se algo se comportar de forma estranha,
  revogue Acessibilidade e rode `./scripts/permissions.sh`.
- `tccutil reset` existe, mas apaga consentimentos de todos os apps daquela
  categoria. Nao e usado pelo script e nao deve ser rotina.
- Nunca contorne o TCC com `osascript` em outro processo para "herdar" a
  permissao. Alem de fragil, e exatamente o padrao de confused deputy descrito
  em `docs/security.md`.
