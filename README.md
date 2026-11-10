# agent-dash

Dashboard de terminal (estilo lazygit) para acompanhar o Claude Code nesta máquina:
quais sessões estão abertas e o que cada uma está fazendo, quantos subagentes rodam
em paralelo, quais shells e monitors estão em background e quantos tokens cada um consome.

```
 agent-dash  4 sessões (2 trabalhando) │ 1 agentes │ 0 shells 1 monitors │ hoje 99.2M tok ▂▂▃▃█▁
┏[1] Sessões (4)━━━━━━━━━━━━━━━━━━━━━━━━━━┓╭ Sessão agent-dash-2d ─ Resumo  Atividade  Processos ─╮
┃● agent-dash-2d     ocupada   5.7M    1m ┃│Status   ● ocupada   pid 7176 · v2.1.319               │
┃● meu-backend-a6    shell   277.9M   13m ┃│Paralelo 0 agentes rodando · 1 task em background      │
┃● meu-site-d9       ociosa   22.7M   25m ┃│╭ Tokens ──────────────╮╭ Ferramentas ────────────────╮│
╭[2] Agentes (0 rodando)──────────────────╮││Cache leitura   5.4M  ││Bash   34 ▇▇▇▇▇▇▇▇▇▇▇▇▇▇▇▇▇  ││
╭[3] Shells/Monitors/Tasks (1 rodando)────╮││Contexto atual 196.4k ││Edit   11 ▇▇▇▇▇▇              ││
│● monitor  erros no log do servidor  …24m │╰──────────────────────╯╰────────────────────────────╯│
╭[4] Uso por dia──────────────────────────╮│╭ Tokens/min · última hora ──────────────────────────╮│
│dom 08/11    99.2M ■■■■■■■■■■            ││                       ▃   ▆▆█▃▃     ██      ▄█▅▅█  ││
```

> **Vai instalar no seu PC?** Siga o [guia de instalação e uso](INSTALL.md).

## Rodar

```powershell
agent-dash            # abre o dashboard
agent-dash --dump     # resumo em texto, sem TUI (útil para scripts/diagnóstico)
agent-dash --stop-task <id>   # para um shell/monitor em background pelo id
agent-dash --help
```

Se o comando não for encontrado, abra um terminal novo (o rustup adiciona
`%USERPROFILE%\.cargo\bin` ao PATH) ou rode `.\target\release\agent-dash.exe`.

## Atalhos

| Tecla | Ação |
|---|---|
| `1` `2` `3` `4` / `Tab` / `h` `l` | painel: Sessões, Agentes, Tasks, Uso por dia |
| | Agentes e Tasks só aparecem quando têm algum item (`a` traz os terminados das últimas 24h); a altura acompanha a quantidade |
| `j` `k` / `↑` `↓` / `g` `G` | mover seleção |
| `[` `]` | trocar aba do painel principal (Resumo / Atividade / Processos, Saída / Detalhes) |
| `J` `K` / `PgUp` `PgDn` | rolar o painel principal (na saída de uma task, sobe a partir do fim) |
| `Enter` | **chat dentro do painel**: entra no chat da sessão selecionada (retoma aqui uma sessão encerrada ou em background) |
| `F12` / `Ctrl+]` | no chat: devolve o teclado ao painel (o chat continua rodando; `▣` marca sessões com chat aberto) |
| `Shift+PgUp` / `Shift+PgDn` | no chat: rolar |
| `Ctrl+Enter` | no chat: quebrar linha sem enviar |
| `z` / `+` | zoom no painel principal (vale para o chat também) |
| `a` | mostrar tudo das últimas 24h (sessões encerradas, agentes e tasks antigos) |
| `f` | agentes/tasks só da sessão selecionada |
| `n` | **nova instância**: escolhe repositório, modelo (padrão/opus/sonnet/haiku/fable), onde abrir (aqui no painel, aba nova do terminal ou background com `claude --bg`), nome e prompt inicial |
| `o` | sessão encerrada: retoma numa aba nova (`claude --resume`); em background: abre (`claude attach`); task: abre a saída no Bloco de Notas |
| `x` | parar o shell/monitor selecionado; encerrar a sessão selecionada (em background: `claude stop`). Sempre pede confirmação |
| `d` / `D` | limpar da lista o item terminado / todos os terminados do painel (sessão em background encerrada: `claude rm`, com confirmação) |
| `u` | desfazer a última limpeza (`a` também mostra os itens limpos) |
| `e` | abrir a pasta da sessão no Explorer |
| `c` | copiar: `claude --resume <id>` (sessão), id do agente, caminho da saída da task |
| `r` | forçar atualização |
| `?` | ajuda |
| `q` / `Ctrl+C` | sair |

## Chat dentro do painel

O agent-dash roda o próprio `claude` num pseudo-terminal (ConPTY) e desenha a tela dele na aba
**Chat** do painel principal, como um tmux. Vários chats ficam abertos ao mesmo tempo; `F12` volta
para a lista e `Enter` em outra sessão troca de conversa.

- Funciona para sessões **abertas pelo dashboard** (`n`), **encerradas** (`claude --resume`) e
  **em background** (`claude attach`).
- **Não** funciona para uma sessão que já está aberta em outra aba do terminal: ela pertence
  àquele terminal. Encerre com `x` e dê `Enter` para retomá-la aqui.
- Fechar o dashboard fecha os chats embutidos (ele pede confirmação). As conversas ficam salvas;
  sessões em background continuam rodando.
- Texto colado com várias linhas vai como um bloco só (bracketed paste), sem enviar no meio.

## Controle (o que dá e o que não dá para fazer daqui)

| Alvo | Parar | Como |
|---|---|---|
| Shell / monitor em background | sim | finaliza o processo do shell e os filhos dele; o Claude Code recebe o aviso de fim da task normalmente (aparece como `✗ falhou`) |
| Sessão interativa | sim | finaliza o processo da sessão e os filhos. A conversa fica salva e pode ser retomada com `o` |
| Sessão em background (`claude --bg`) | sim | `claude stop <id>`; remover com `claude rm <id>`. Iniciar em background exige que a pasta já seja confiável (abra `claude` nela uma vez antes) |
| Subagente / workflow | **não** | rodam dentro do processo da sessão e não há como parar só um de fora. Use TaskStop na própria sessão ou encerre a sessão |

**Como o processo de uma task é identificado**: o Claude Code não grava o PID das tasks.
O agent-dash procura, entre os shells filhos da sessão, aquele cujo comando bate com o da task
e que nasceu entre a chamada da ferramenta e o resultado dela. Tasks do PowerShell passam o
script por variável de ambiente, então nelas só vale o horário. Se houver mais de um candidato,
a ação é recusada em vez de arriscar o processo errado.

Itens limpos (`d`) ficam em `%LOCALAPPDATA%gent-dash\hidden.json`; nada é apagado de `~/.claude`.

## De onde vêm os dados

Tudo é lido localmente, só leitura, de `~/.claude` (ou `$CLAUDE_CONFIG_DIR`):

| Dado | Fonte |
|---|---|
| Sessões abertas, status (ociosa/ocupada/shell), PID, nome | `sessions/<pid>.json`; o PID é conferido na lista de processos para descartar arquivos velhos |
| Sessões em background (`claude --bg`) | `claude agents --json --all`, a cada 30 s numa thread à parte (e logo depois de cada ação ou `r`) |
| Repositórios para "nova instância" | diretórios das sessões + projetos de `~/.claude.json` que ainda existem |
| Tokens, modelo, ferramentas usadas, atividade | `projects/<projeto>/<sessionId>.jsonl` |
| Subagentes (inclusive de workflows) | `projects/<projeto>/<sessionId>/subagents/**/agent-*.jsonl` + `.meta.json` |
| Shells, monitors, workflows e agentes em background | início no `toolUseResult` (`backgroundTaskId`, `taskId`, `agentId`); fim na `<task-notification>` |
| Saída de cada task | `%TEMP%\claude\*\<sessionId>\tasks\<id>.output` |
| CPU, memória e processos filhos | árvore de processos do sistema (`sysinfo`) |

Os transcripts são lidos de forma incremental (só as linhas novas a cada segundo).
Na abertura, as sessões vivas são lidas primeiro e o resto do histórico carrega em
segundo plano (o cabeçalho mostra "lendo histórico N%").

### Como os tokens são contados

- **Total** = entrada + cache escrita + cache leitura + saída. É o volume que passou pela API.
  Cache leitura costuma dominar, porque o contexto inteiro é relido a cada turno.
- **Contexto atual** = entrada + cache da resposta mais recente (o tamanho da conversa agora).
- Uma resposta da API é gravada em várias linhas com o mesmo `message.id`; o agent-dash
  deduplica por id (e entre arquivos, para sessões retomadas não contarem em dobro).
- Não há estimativa de custo em dólar, só tokens.

### Heurísticas (podem errar em casos raros)

- **Agente rodando**: sem `finished` no meta, sem resultado no transcript do pai e a sessão
  dona viva (e ocupada, para agentes em primeiro plano).
- **Monitor expirado**: monitors têm prazo (`timeoutMs`); passou do prazo sem notificação,
  aparece como `◌ expirado`.
- **Task órfã** (`?`): sem notificação de fim e a sessão dona já fechou.

## Compilar

Requer Rust (testado com 1.99, toolchain `x86_64-pc-windows-gnu`).

```powershell
cargo build --release
cargo install --locked --path .   # instala em %USERPROFILE%\.cargo\bin
```

**Por que algumas dependências estão fixadas em versões antigas** (`chrono =0.4.39`,
`dirs 5`, `parking_lot 0.12.3`/`parking_lot_core 0.9.10` no `Cargo.lock`): as versões novas usam
`windows-link`, que exige `dlltool` + assembler (binutils) no toolchain GNU. As versões
fixadas trazem as import libs prontas, então o build funciona só com o rustup, sem Visual Studio
C++ nem MinGW. Por isso o `cargo install` precisa de `--locked` (sem ele o cargo ignora o
`Cargo.lock` e o build falha com `error calling dlltool`). Se você instalar o workload "Desenvolvimento para desktop com C++" e usar o
toolchain MSVC, pode atualizar tudo com `cargo update`.

## Estrutura

```
src/
  main.rs       loop da TUI, --dump, --stop-task e --frame (render em memória, para depuração)
  term.rs       chat embutido: pseudo-terminal (portable-pty) + emulador VT (vt100)
  actions.rs    ações: parar task/sessão, abrir/retomar/iniciar instâncias (wt, claude CLI)
  collector.rs  thread de coleta: sessões, processos, descoberta de arquivos, snapshot
  scan.rs       leitura incremental dos .jsonl (tokens, eventos, tasks)
  model.rs      tipos compartilhados
  app.rs        estado da UI e teclas
  ui.rs         renderização (ratatui)
```
