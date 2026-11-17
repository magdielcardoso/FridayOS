# FridayOS — regras para agentes

Regras operacionais deste repositório. Valem para qualquer agente (build, dev,
review) que trabalhe aqui.

## Não reinicie a Friday por conta própria

- É proibido rodar `pkill -f "MacOS/FridayOS"`, `killall FridayOS`,
  `open -n`, ou reinstalar `/Applications/FridayOS.app` no meio de uma tarefa.
- `open -n` abre **sempre** uma cópia nova. Use `open dist/FridayOS.app`
  (sem `-n`): o app tem guarda de instância única e a segunda cópia sai.
- Nunca deixe loop (`while`, `watch`, script em background) que recompile,
  reinstale ou reabra o app. Se precisar de build, rode `./scripts/bundle.sh`
  uma vez e diga ao usuário o que mudou.
- Testes que abrem/fecham a interface são caros e já causaram loop de
  "o app abre sozinho": combine com o usuário antes.
- O dev loop oficial é `./scripts/dev.sh`. Ele segura um lock
  (`.dev-loop.lock`) e recusa uma segunda instância — não contorne o lock.

## Estado do usuário

- `~/Library/Application Support/FridayOS/` (memória, histórico, contas,
  audit) é dado real do usuário. Nunca apague nem sobrescreva em massa.
- `/Applications/FridayOS.app` é a instalação do usuário. Não substitua sem
  pedir.

## Build

- `./scripts/bundle.sh` gera `dist/FridayOS.app` com o Info.plist de TCC.
- `cargo build -p friday-app -p friday-tools` para validar compilação.
- Rode `cargo test -p friday-core -p friday-tools` antes de concluir.
