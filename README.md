# agent-dash

Dashboard de terminal (estilo lazygit) para acompanhar o Claude Code nesta máquina:
quais sessões estão abertas e o que cada uma está fazendo, quantos subagentes rodam
em paralelo, quais shells e monitors estão em background e quantos tokens cada um consome.

Também abre e acompanha outros agentes de linha de comando: as sessões do **Antigravity**
(`agy`, para usar o Gemini) aparecem ao lado das do Claude, com tokens, e o **Codex** roda
dentro do painel. Ver [Outros agentes](#outros-agentes-antigravity-codex).

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

## Instalação

Precisa de Windows 10 ou 11 e do Claude Code no PATH. **Não precisa de Rust:** cada release
traz o `agent-dash.exe` pronto. O passo a passo completo está no
[guia de instalação e uso](INSTALL.md).

**1. Baixe o executável e coloque no PATH.** Cole num PowerShell:

```powershell
$dir = "$env:LOCALAPPDATA\Programs\agent-dash"
New-Item -ItemType Directory -Force $dir | Out-Null
Invoke-WebRequest https://github.com/Antonio-Ramon/agent-dash/releases/latest/download/agent-dash.exe -OutFile "$dir\agent-dash.exe"
$p = [Environment]::GetEnvironmentVariable("Path", "User")
if ($p -notlike "*$dir*") { [Environment]::SetEnvironmentVariable("Path", "$p;$dir", "User") }
```

**2. Confira** num terminal novo (o PATH novo só vale nos terminais abertos depois):

```powershell
agent-dash --dump
```

Deu erro? Veja os [problemas comuns](INSTALL.md#8-problemas-comuns).

Depois de instalado, o agent-dash **se atualiza sozinho**: ao abrir, confere o release mais recente
no GitHub e, se houver versão nova, baixa o executável, troca o atual e reabre
([guia, 6](INSTALL.md#6-atualizar)).

Quer compilar a partir do código? Aí sim precisa de Rust: veja
[Compilar a partir do código](#compilar-a-partir-do-código).

## Rodar

```powershell
agent-dash            # abre o dashboard
agent-dash --dump     # resumo em texto, sem TUI (útil para scripts/diagnóstico)
agent-dash --stop-task <id>   # para um shell/monitor em background pelo id
agent-dash --help
```

Se o comando não for encontrado, abra um terminal novo: o PATH só muda nos terminais abertos
depois da instalação.

## Atalhos

| Tecla | Ação |
|---|---|
| `1` `2` `3` `4` / `Tab` / `h` `l` | painel: Sessões, Agentes, Tasks, Uso por dia |
| | os quatro painéis da esquerda têm altura fixa (não mudam com a quantidade de itens); lista maior que o painel rola, e o painel em foco mostra a barra de rolagem |
| mouse | **roda**: rola o que está sob o cursor (lista, painel principal ou chat) sem mudar foco nem seleção; no chat a roda vai para o Claude, que rola a conversa como num terminal comum; **clique**: dá foco ao painel e seleciona a linha; clique no chat entrega o teclado a ele; **arrastar no chat** seleciona o texto e copia ao soltar (`Ctrl+C` com seleção copia de novo em vez de interromper o Claude). Fora do chat, para selecionar texto segure `Shift` |
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
| `v` | uso de tokens: todos os agentes ou um só (ver [Uso por agente](#uso-por-agente)) |
| `n` | **nova instância**: escolhe repositório, agente (Claude, Antigravity, Codex ou um configurado por você, ver [Outros agentes](#outros-agentes-antigravity-codex)), modelo, onde abrir (aqui no painel, aba nova do terminal ou, só no Claude, background com `claude --bg`), nome e prompt inicial |
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

Os chats abertos ficam anotados em `%LOCALAPPDATA%\agent-dash\open_chats.json` a cada mudança. Se o
dashboard fechar com chats abertos (`q`, janela fechada, queda), na próxima abertura ele pergunta se
é para **reabrir todos** (`claude --resume` em cada um). Chat encerrado por você sai da lista; attach
em sessão de background não entra, porque ela continua rodando sozinha.

- Funciona para sessões **abertas pelo dashboard** (`n`), **encerradas** (`claude --resume`) e
  **em background** (`claude attach`).
- **Não** funciona para uma sessão que já está aberta em outra aba do terminal: ela pertence
  àquele terminal. Encerre com `x` e dê `Enter` para retomá-la aqui.
- Fechar o dashboard fecha os chats embutidos (ele pede confirmação). As conversas ficam salvas;
  sessões em background continuam rodando.
- Texto colado com várias linhas vai como um bloco só (bracketed paste), sem enviar no meio.

## Outros agentes (Antigravity, Codex…)

No `n`, o campo **Agente** abre outro agente de linha de comando (Antigravity, Codex) no lugar do Claude Code, aqui no
painel ou numa aba nova. Ele aparece em **[1] Sessões** com `◆`, `Enter` entra no chat dele, `F12`
volta e `x` fecha. O formulário procura cada agente no PATH ao abrir: o que não está instalado
aparece riscado, com o comando para instalar.

| Agente | Comando | Instalar |
|---|---|---|
| Antigravity | `agy` | Antigravity CLI do Google |
| Codex | `codex` | `npm install -g @openai/codex` |

O Gemini CLI (`gemini`) não vem na lista: ele não aceita mais login com conta Google pessoal e o
Google manda migrar para o Antigravity. Quem usa o Gemini CLI com chave de API ou Vertex AI pode
incluí-lo no `agentes.json` (abaixo).

A lista de modelos do Antigravity saiu de `agy models` (nov/2026) e muda com o tempo: se um
modelo sumir, troque a lista no `agentes.json`.

### Sessões do Antigravity

As sessões do Antigravity aparecem em **[1] Sessões** como as do Claude, abertas em qualquer
lugar (outra aba do terminal, outro dashboard), não só as abertas pelo `n`:

| | Como funciona |
|---|---|
| Lista | vivas sempre; encerradas das últimas 24h com `a` (igual ao Claude) |
| Viva / encerrada | a conversa está viva enquanto um `agy` trava `presence/<id>.lock`; quem trava sai da Restart Manager do Windows, que só consulta o arquivo |
| Ocupada / ociosa | ocupada se a conversa (`conversations/<id>.db` ou o `-wal` dela) foi gravada nos últimos 8 s |
| Tokens | lidos da própria conversa: cada resposta do modelo grava entrada, saída e cache (ver [Tokens do Antigravity](#tokens-do-antigravity)) |
| Sessão nova | um `agy` aberto sem nenhuma mensagem ainda não tem conversa: aparece como **antigravity (nova)** pelo processo, e passa a ser a conversa na primeira mensagem |
| Título, pasta, prompts | `cache/conversation_metadata.json`, `history.jsonl` e a pasta de trabalho do processo |
| `Enter` | encerrada: retoma aqui no painel (`agy --conversation <id>`); aberta em outro terminal: avisa, como no Claude |
| `o` | encerrada: retoma numa aba nova |
| `x` | encerra o processo (aqui no painel ou em outro terminal), sempre com confirmação |
| `c` | copia `agy --conversation <id>` |
| Reabrir ao abrir o dashboard | conversas do Antigravity abertas aqui entram na lista de reabrir, como as do Claude |

O que continua só no Claude: subagentes, tasks e background (`--bg`).

O Codex só aparece enquanto roda num chat aberto pelo `n`: o dashboard ainda não lê as sessões
nem os tokens dele (a coluna de tokens mostra `—`).

**Prompt inicial em agentes instalados pelo npm**: eles rodam por um atalho `.cmd`, que passa
pelo `cmd.exe`. Um prompt com `" % & | < > ^` é recusado (pode cortar o comando ou rodar outro);
abra sem prompt e mande a mensagem no chat.

### Configurar agentes

Crie `%LOCALAPPDATA%\agent-dash\agentes.json` para trocar os modelos oferecidos ou incluir outro
agente. Um item com o mesmo `nome` de um de fábrica (Antigravity, Codex) substitui o original; um nome
novo entra no fim da lista. O Claude não pode ser substituído.

```json
[
  {
    "nome": "Antigravity",
    "comando": "agy",
    "modelos": ["gemini-3.8-flash-high", "claude-opus-4-6-thinking"],
    "flag_modelo": "--model",
    "flag_prompt": "--prompt-interactive",
    "flag_retomar": "--conversation"
  },
  {
    "nome": "Gemini",
    "comando": "gemini",
    "modelos": ["gemini-2.5-pro", "gemini-3.5-flash"],
    "flag_modelo": "--model",
    "flag_prompt": "--prompt-interactive",
    "instalar": "npm install -g @google/gemini-cli"
  },
  { "nome": "Aider", "comando": "aider", "args": ["--no-auto-commits"], "flag_prompt": "--message" }
]
```

| Campo | O que é |
|---|---|
| `nome` | nome no formulário e na lista de sessões |
| `comando` | programa a rodar, procurado no PATH (ou caminho completo) |
| `args` | argumentos fixos, sempre passados |
| `modelos` | opções do campo Modelo além de "padrão" (que não passa nada) |
| `flag_modelo` | flag que recebe o modelo; sem ela não há escolha de modelo |
| `flag_prompt` | flag que recebe o prompt inicial; sem ela o prompt vai como argumento solto |
| `instalar` | comando mostrado quando o agente não está instalado |
| `flag_retomar` | flag que recebe o id de uma conversa para retomá-la (`Enter` e `o` numa conversa encerrada, reabrir ao abrir o dashboard); sem ela a conversa não é retomada pelo dashboard |

Um item que substitui um agente de fábrica substitui **tudo**: para manter o Antigravity
retomando conversas, inclua `"flag_retomar": "--conversation"` no item dele.

Se o arquivo tiver erro de JSON, ele é ignorado e o aviso aparece no rodapé ao abrir o dashboard.

## Controle (o que dá e o que não dá para fazer daqui)

| Alvo | Parar | Como |
|---|---|---|
| Shell / monitor em background | sim | finaliza o processo do shell e os filhos dele; o Claude Code recebe o aviso de fim da task normalmente (aparece como `✗ falhou`) |
| Sessão interativa | sim | finaliza o processo da sessão e os filhos. A conversa fica salva e pode ser retomada com `o` |
| Sessão do Antigravity | sim | finaliza o `agy` dono da conversa e os filhos, aqui no painel ou em outro terminal (antes confere que o PID ainda é um `agy`). A conversa fica salva e volta com `Enter` ou `o` |
| Chat de outro agente aberto pelo `n` (Codex…) | sim | `x` fecha o chat e finaliza o processo |
| Sessão em background (`claude --bg`) | sim | `claude stop <id>`; remover com `claude rm <id>`. Iniciar em background exige que a pasta já seja confiável (abra `claude` nela uma vez antes) |
| Subagente / workflow | **não** | rodam dentro do processo da sessão e não há como parar só um de fora. Use TaskStop na própria sessão ou encerre a sessão |

**Como o processo de uma task é identificado**: o Claude Code não grava o PID das tasks.
O agent-dash procura, entre os shells filhos da sessão, aquele cujo comando bate com o da task
e que nasceu entre a chamada da ferramenta e o resultado dela. Tasks do PowerShell passam o
script por variável de ambiente, então nelas só vale o horário. Se houver mais de um candidato,
a ação é recusada em vez de arriscar o processo errado.

Itens limpos (`d`) ficam em `%LOCALAPPDATA%\agent-dash\hidden.json`; nada é apagado de `~/.claude`.

## De onde vêm os dados

Tudo é lido localmente, só leitura, de `~/.claude` (ou `$CLAUDE_CONFIG_DIR`):

| Dado | Fonte |
|---|---|
| Sessões abertas, status (ociosa/ocupada/shell), PID, nome | `sessions/<pid>.json`; o PID é conferido na lista de processos para descartar arquivos velhos |
| Sessões em background (`claude --bg`) | `claude agents --json --all`, a cada 30 s numa thread à parte (e logo depois de cada ação ou `r`) |
| Sessões do Antigravity | `~/.gemini/antigravity-cli` (`conversations/`, `presence/`, `cache/`, `history.jsonl`) + Restart Manager do Windows para saber qual `agy` está com cada conversa (a cada 5 s, só se houver `agy` rodando) |
| Tokens do Antigravity | tabelas `steps` e `gen_metadata` de `conversations/<id>.db` (e do `-wal`), relidas só quando o arquivo muda |
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

### Uso por agente

Com mais de um agente consumindo tokens, o uso aparece separado por agente, porque cada um tem
a sua cota e o seu tokenizador (1M de tokens do Gemini não equivale a 1M do Claude):

| Onde | O que mostra |
|---|---|
| Cabeçalho | `hoje Claude 37.4M · Antigravity 120k`, um número por agente (só o total enquanto só o Claude foi usado) |
| [4] Uso por dia e gráfico do painel de uso | barras empilhadas, uma cor por agente (Claude em magenta, Antigravity em azul, Codex em verde) |
| Tabela do dia | modelos agrupados por agente, com o subtotal de cada um |
| `v` | alterna entre todos os agentes e cada um sozinho (cabeçalho, barras, totais e tabela) |

Uma parte de barra que não é zero ganha pelo menos 1/8 de linha, para o agente pequeno não sumir
ao lado de um grande.

### Tokens do Antigravity

O `agy` grava cada resposta do modelo como um passo na tabela `steps` da conversa, com o consumo
num protobuf sem esquema publicado. Os campos foram identificados comparando conversas reais
(`agy` 1.1): entrada, saída, leitura e escrita de cache, e a saída dividida em raciocínio e
resposta. O nome do modelo vem da `gen_metadata`; um modelo sem nome lá (o auxiliar que gera
títulos) aparece como `antigravity #<id>`.

Como o formato não é documentado, cada resposta é conferida: a saída tem que ser exatamente
raciocínio + resposta. Se uma conferência falhar, o formato mudou e a conversa inteira fica sem
tokens (`—`), em vez de mostrar números errados. O banco é lido por um leitor de SQLite próprio
(`sqlite.rs`, só leitura, aplica o `-wal`), sem biblioteca em C.

### Heurísticas (podem errar em casos raros)

- **Agente rodando**: sem `finished` no meta, sem resultado no transcript do pai e a sessão
  dona viva (e ocupada, para agentes em primeiro plano).
- **Monitor expirado**: monitors têm prazo (`timeoutMs`); passou do prazo sem notificação,
  aparece como `◌ expirado`.
- **Task órfã** (`?`): sem notificação de fim e a sessão dona já fechou.

## Compilar a partir do código

Requer Rust 1.88 ou mais novo pelo rustup (testado com 1.99, toolchain `x86_64-pc-windows-gnu`).
Para preparar o Rust (qual variante usar e como conferir a sua), veja o
[guia, seção 2](INSTALL.md#2-preparar-o-rust).

```powershell
cargo build --release              # gera target\release\agent-dash.exe
cargo install --locked --path .   # instala em %USERPROFILE%\.cargo\bin
```

Compile pelo PowerShell. No Git Bash, o toolchain MSVC encontra o `link` do próprio Git
(um utilitário de arquivos) no lugar do linker da Microsoft e falha com `link: extra operand`.

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
  antigravity.rs sessões do Antigravity CLI: conversas, quem está com cada uma aberta, títulos, prompts e tokens
  sqlite.rs     leitor mínimo de SQLite (só leitura, com WAL) para as conversas do Antigravity
  agents.rs     outros agentes (Antigravity, Codex…): lista de fábrica, agentes.json, busca no PATH
  collector.rs  thread de coleta: sessões, processos, descoberta de arquivos, snapshot
  scan.rs       leitura incremental dos .jsonl (tokens, eventos, tasks)
  model.rs      tipos compartilhados
  app.rs        estado da UI e teclas
  ui.rs         renderização (ratatui)
  i18n.rs       idioma da interface (pt/en, pelo idioma do Windows)
```

## Idioma

A interface segue o idioma de exibição do Windows: português em sistemas pt-*,
inglês nos demais. Para forçar um deles, defina `AGENT_DASH_LANG=pt` ou `AGENT_DASH_LANG=en`.

## Licença

MIT. Ver [LICENSE](LICENSE). Autores: Antonio Ramon e Mateus Cristo.
A ajuda do dashboard (`?`) e o `agent-dash --help` mostram versão, autores e licença.
