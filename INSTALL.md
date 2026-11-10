# agent-dash: instalação e uso

Guia para quem baixou o repositório e quer ter o `agent-dash` no próprio PC.
Para detalhes de funcionamento (de onde vêm os dados, como os tokens são contados), veja o [README](README.md).

## 1. O que você precisa

| Requisito | Por quê | Como conferir |
|---|---|---|
| **Windows 10 ou 11** | O dashboard usa recursos do Windows (pseudo-terminal ConPTY, área de transferência, Explorer). Não foi testado em Linux nem macOS. | — |
| **Claude Code** instalado e no PATH | É ele que o dashboard monitora e abre. | `claude --version` |
| **Git** | Para baixar o repositório. | `git --version` |
| **Rust 1.88 ou mais novo, pelo rustup** (toolchain GNU recomendado) | Para compilar. Ter "algum" Rust não basta: a seção 2 mostra como conferir. | `where.exe cargo` e `rustc -vV` |
| **Windows Terminal** (recomendado) | Cores, símbolos e a opção "aba nova" funcionam melhor nele. Sem ele, "aba nova" abre uma janela de console comum. | `wt --version` |

Com o toolchain GNU, não precisa de Visual Studio nem de MinGW.

## 2. Preparar o Rust

O agent-dash compila com o Rust na variante **GNU** (`x86_64-pc-windows-gnu`), que não depende do
Visual Studio. A variante **MSVC** também funciona, mas só se o Visual Studio tiver o componente
"Desenvolvimento para desktop com C++". Ter o Visual Studio instalado sem esse componente não basta:
falta o linker (`link.exe`) e o Windows SDK.

### 2.1 Descubra o que você tem

Abra um PowerShell **novo** e rode:

```powershell
where.exe cargo
rustc -vV | Select-String host
```

No PowerShell, `where` sozinho é outro comando; use `where.exe`.

Compare com a tabela de cima para baixo e pare na primeira linha que bater:

| O que aparece | Situação | O que fazer |
|---|---|---|
| `where.exe` não encontra `cargo` | Sem Rust | [2.2](#22-instalar-o-rustup-com-o-toolchain-gnu) |
| A primeira linha é `C:\Program Files\Rust ...\bin\cargo.exe` | Rust do instalador MSI. Vem na variante MSVC, sem rustup, e não dá para trocar de variante | [2.3](#23-rust-instalado-pelo-msi) |
| A primeira linha é `...\.cargo\bin\cargo.exe` e `host: x86_64-pc-windows-msvc` | rustup na variante MSVC | [2.4](#24-rustup-na-variante-msvc) |
| A primeira linha é `...\.cargo\bin\cargo.exe` e `host: x86_64-pc-windows-gnu` | Pronto | Siga para a [seção 3](#3-baixar-e-instalar-o-agent-dash) |

### 2.2 Instalar o rustup com o toolchain GNU

1. Baixe o `rustup-init.exe` (64-bit) em <https://rustup.rs>.
2. Rode no PowerShell, na pasta onde baixou:

   ```powershell
   .\rustup-init.exe -y --default-host x86_64-pc-windows-gnu
   ```

   O `--default-host x86_64-pc-windows-gnu` escolhe a variante que compila sem o Visual Studio C++.
   Não precisa de administrador: tudo vai para `%USERPROFILE%\.cargo` e `%USERPROFILE%\.rustup`.
3. **Feche e abra o terminal** e repita a conferência da [2.1](#21-descubra-o-que-você-tem). Tem que dar
   `.cargo\bin` e `host: x86_64-pc-windows-gnu`.

### 2.3 Rust instalado pelo MSI

O instalador MSI coloca o Rust no PATH do **sistema**. O rustup coloca `%USERPROFILE%\.cargo\bin` no PATH
do **usuário**. O Windows monta o PATH com o do sistema primeiro, então, com os dois instalados, o
`cargo` do MSI continua sendo o usado e o build falha do mesmo jeito.

1. Desinstale o Rust do MSI em **Configurações > Apps > Aplicativos instalados**. A entrada se chama
   algo como "Rust 1.99 (MSVC 64-bit)".
2. Siga a [2.2](#22-instalar-o-rustup-com-o-toolchain-gnu). Se instalar o rustup antes de desinstalar o
   MSI, o `rustup-init` avisa que já existe outro Rust; com `-y` ele instala mesmo assim.
3. Repita a conferência da [2.1](#21-descubra-o-que-você-tem) num terminal novo.

Se não puder desinstalar o MSI (máquina gerenciada, por exemplo), instale o rustup e ponha o
`.cargo\bin` na frente do PATH só na sessão em que for compilar:

```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
rustc -vV | Select-String host    # tem que dar x86_64-pc-windows-gnu
cargo install --locked --path .
```

### 2.4 rustup na variante MSVC

Se o Visual Studio não tem o componente de C++, troque para o GNU (o rustup baixa o toolchain se
ainda não tiver):

```powershell
rustup default stable-x86_64-pc-windows-gnu
```

Se você tem o componente de C++ e prefere a variante MSVC, pode usar como está. Só compile pelo
PowerShell (veja a seção 3).

## 3. Baixar e instalar o agent-dash

Pelo **PowerShell**:

```powershell
git clone https://github.com/Antonio-Ramon/agent-dash.git
cd agent-dash
cargo install --locked --path .
```

- A compilação leva de 1 a 3 minutos na primeira vez.
- Evite o Git Bash para compilar: nele, o toolchain MSVC encontra o `link` do próprio Git (um
  utilitário de arquivos) no lugar do linker da Microsoft e falha com `link: extra operand`.
- O executável vai para `%USERPROFILE%\.cargo\bin\agent-dash.exe`, que o rustup já colocou no PATH.
- **O `--locked` é obrigatório.** Sem ele o cargo pega versões mais novas de algumas dependências e a compilação falha com `error calling dlltool` (veja "Problemas comuns").

Confira:

```powershell
agent-dash --help
agent-dash --dump     # resumo em texto das suas sessões, sem abrir a interface
```

## 4. Primeiro uso

Abra um terminal (de preferência o Windows Terminal, com a janela grande) e rode:

```powershell
agent-dash
```

Na primeira abertura o cabeçalho mostra "lendo histórico N%" enquanto o histórico de tokens carrega em segundo plano. As sessões abertas aparecem na hora.

### A tela

```
 agent-dash  5 sessões (2 trabalhando) │ 0 agentes │ 0 shells 1 monitors │ hoje 145.9M tok ▂▃█
┏[1] Sessões ━━━━━━━━━━┓╭ Sessão … ─ Resumo  Atividade  Processos  Chat ─────────────╮
┃● minha-sessao  ocupada┃│                                                            │
┗━━━━━━━━━━━━━━━━━━━━━━┛│   painel principal: detalhes do item selecionado           │
╭[2] Agentes ──────────╮│   (ou o chat da sessão)                                    │
╭[3] Shells/Monitors ──╮│                                                            │
╭[4] Uso por dia ──────╮╰────────────────────────────────────────────────────────────╯
 1-4 painel  ↑↓ mover  [ ] aba  z zoom  …  ? ajuda  q sair
```

- **Esquerda**: quatro painéis. `1`, `2`, `3`, `4` (ou `Tab`) escolhem o painel; `↑` `↓` (ou `j` `k`) movem a seleção.
- **Direita**: detalhes do item selecionado. `[` e `]` trocam de aba.
- **Rodapé**: os atalhos que valem para o painel atual. `?` mostra todos.

### O que os símbolos querem dizer

| Símbolo | Significado |
|---|---|
| `●` amarelo | sessão ocupada / agente ou task rodando |
| `●` verde | sessão ociosa |
| `●` ciano | sessão rodando um comando de shell |
| `◐` | sessão em background parada, esperando uma resposta sua |
| `○` | sessão encerrada (aparece com `a`) |
| `▣` | sessão com chat aberto dentro do dashboard |
| `⚑n` / `⚙n` | agentes / tasks rodando naquela sessão |
| `✓` `✗` `◌` `?` | task concluída / interrompida ou com falha / monitor expirado / órfã |

## 5. Tarefas do dia a dia

### Acompanhar o consumo
- Painel `1`: tokens de cada sessão; a aba **Resumo** detalha entrada, cache, saída e contexto atual.
- Painel `4`: tokens por dia; `↑` `↓` escolhem o dia e a direita mostra o consumo por modelo.

### Abrir uma instância nova
1. Aperte `n`.
2. Escolha o repositório (digite para filtrar, `↑` `↓` para escolher) ou digite um caminho.
3. `Tab` vai para os outros campos: modelo, modo (**aqui no painel**, aba nova do terminal ou background), nome e prompt inicial. `←` `→` trocam modelo e modo.
4. `Enter` inicia.

### Conversar dentro do dashboard
- `Enter` numa sessão entra no chat dela. Funciona para sessões abertas pelo dashboard, encerradas (são retomadas) e em background.
- `F12` devolve o teclado ao dashboard; o chat continua rodando. Para trocar de conversa: `F12`, escolha outra sessão, `Enter`.
- `Ctrl+Enter` quebra linha; `Shift+PgUp` / `Shift+PgDn` rolam o chat; `z` (fora do chat) deixa o painel em tela cheia.
- A linha embaixo do chat mostra o contexto atual e os tokens gastos pela sessão.
- Uma sessão que já está aberta em **outra aba do terminal** não pode ser trazida para dentro: encerre-a com `x` e dê `Enter` para retomá-la aqui.

### Parar e limpar
- `x` num shell ou monitor (painel `3`) para o processo. `x` numa sessão (painel `1`) encerra a sessão; a conversa fica salva.
- `d` tira da lista um item já terminado; `D` tira todos os terminados do painel; `u` desfaz.
- Tudo que mata processo pede confirmação (`y` / `n`).
- Subagentes e workflows não podem ser parados individualmente: eles rodam dentro do processo da sessão.

### Outros atalhos úteis
| Tecla | Ação |
|---|---|
| `a` | mostrar também o que já terminou nas últimas 24h |
| `f` | agentes e tasks só da sessão selecionada |
| `o` | sessão: abrir numa aba nova do terminal · task: abrir a saída no Bloco de Notas |
| `e` | abrir a pasta da sessão no Explorer |
| `c` | copiar (`claude --resume <id>`, id do agente, caminho da saída) |
| `r` | forçar atualização |
| `q` | sair (com chats abertos, pede confirmação: eles fecham junto) |

## 6. Atualizar

Feche o `agent-dash` antes (o Windows não deixa substituir um executável em uso) e rode, na pasta do repositório:

```powershell
git pull
cargo install --locked --path .
```

## 7. Desinstalar

```powershell
cargo uninstall agent-dash
Remove-Item -Recurse "$env:LOCALAPPDATA\agent-dash"   # lista de itens limpos com `d`
```

O agent-dash só lê `~/.claude`; desinstalar não apaga nenhuma conversa.

## 8. Problemas comuns

| Sintoma | Causa e solução |
|---|---|
| `agent-dash` não é reconhecido como comando | Abra um terminal novo. Se continuar, confira se `%USERPROFILE%\.cargo\bin` está no PATH. |
| `error calling dlltool 'dlltool.exe': program not found` ao compilar | Faltou o `--locked`. Rode `cargo install --locked --path .`. Não rode `cargo update`: ele desfaz as versões fixadas no `Cargo.lock`. |
| `error: linker 'link.exe' not found` ao compilar | Seu Rust está na variante MSVC sem o componente de C++ do Visual Studio. Rode `rustup default stable-x86_64-pc-windows-gnu` e instale de novo. Se `rustup` não for reconhecido, seu Rust veio do MSI: veja a [2.3](#23-rust-instalado-pelo-msi). |
| `link: extra operand '...rcgu.o'` ao compilar | Você compilou pelo Git Bash com o toolchain MSVC, e o `link` chamado foi o do Git. Mude para o toolchain GNU (seção 2) ou compile pelo PowerShell. |
| `rustup` não é reconhecido como comando | Ou o terminal é anterior à instalação (abra um novo), ou seu Rust veio do instalador MSI, que não traz rustup. Veja a [2.3](#23-rust-instalado-pelo-msi). |
| Instalei o rustup, mas o build falha igual | O `cargo` do MSI continua vindo antes no PATH. `where.exe cargo` mostra qual está sendo usado; veja a [2.3](#23-rust-instalado-pelo-msi). |
| "o dashboard precisa de um terminal interativo" | Você rodou com a saída redirecionada ou de dentro de outra ferramenta. Rode direto no terminal, ou use `agent-dash --dump`. |
| Símbolos aparecem como quadrados ou a tela fica desalinhada | Use o Windows Terminal com uma fonte como Cascadia Mono/Code. |
| Nenhuma sessão aparece | Confira se há um Claude Code aberto (`claude agents`). Se você usa `CLAUDE_CONFIG_DIR`, o agent-dash precisa da mesma variável. |
| "Workspace not trusted" ao iniciar em background | Abra `claude` uma vez naquela pasta e aceite o aviso de confiança; depois tente de novo. |
| `x` num shell responde "não dá para saber qual é com segurança" | Há mais de um processo que combina com aquela task. O dashboard prefere não arriscar o processo errado; pare pela própria sessão. |
| O chat parece lento logo depois de abrir | O Claude Code ainda está carregando plugins e MCPs; costuma normalizar em alguns segundos. |
