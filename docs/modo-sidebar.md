# Modo Sidebar — discussão, decisões e plano

> Documento de trabalho. Consolida a discussão sobre o recurso "Modo Sidebar" antes da
> implementação. Nada aqui foi codado ainda.

## 1. A ideia

Um painel lateral do WhatsApp que aparece com um atalho global, ancorado na borda da tela,
no espírito do painel lateral do Windows 8 (overlay por cima das outras janelas), e não do
sidebar do Firefox/Chrome (que vive dentro da janela do navegador).

## 2. Onde isso encaixa no código atual

| O que já existe | Onde |
| --- | --- |
| Janela única `main`, criada com decoração e `min_inner_size(780, 480)` | `src-tauri/src/lib.rs:1749` |
| Fechar não encerra, só esconde na bandeja | `install_close_to_tray`, `lib.rs:1737` |
| WebView2 vivo em background com flags anti-throttling | `WEBVIEW2_BROWSER_ARGS`, `lib.rs:45` |
| Modo de baixo consumo quando escondida (EcoQoS + memory target LOW) | `apply_low_power_mode`, `lib.rs:1790` |
| Reload adaptativo por pressão de heap | `due_reload`, `lib.rs:1664` |
| Guarda de chamada ativa (WebRTC) | `set_call_active`, `lib.rs:216` |
| Atalhos in-page (Ctrl+W, Ctrl+↑, Ctrl+Shift+E, Ctrl+Alt+Q) — só com a janela focada | patch JS, `lib.rs:1162` |
| Foco/abertura da janela vindo de notificação e do tray | `focus_main_window`, `lib.rs:244` |
| Tray com menu (Abrir / autostart / Sair) | `run()`, `lib.rs:2010` |

## 3. Conclusões técnicas da discussão

### 3.1 O recurso não custa RAM extra

A premissa inicial ("vai consumir mais RAM, então limitamos") está invertida. O app **já**
mantém o WebView2 carregado e escondido o tempo todo — é por isso que ele abre instantâneo.
Modo sidebar bem feito é a **mesma janela `main`** com outra geometria e outro estilo:
sem decoração, ancorada na borda, `always_on_top`, `skip_taskbar`. Nenhum processo novo,
nenhuma webview nova. Custo marginal ≈ 0 MB.

Só haveria custo real se fosse criada uma **segunda** janela com uma segunda webview — seria
uma cópia inteira do WhatsApp Web (~250–400 MB). Descartado: uma janela, dois modos.

Sobre impor teto de RAM ao WebView2: não existe caminho bom. Job Object com `ProcessMemoryLimit`
existe, mas ao bater o limite a alocação falha e o renderer morre — não degrada suavemente.
O que o projeto já faz é a resposta certa: `MEMORY_USAGE_TARGET_LEVEL_LOW` quando escondido +
reload adaptativo por heap. O modo sidebar herda isso de graça.

> Nota: "RAM mínima", na proposta original, era sobre requisito de RAM instalada na máquina do
> usuário, não sobre limitar o processo. Com custo marginal ≈ 0, o ponto caiu por si.

### 3.2 Atalho global é dependência nova

Os atalhos atuais são JS injetado e só funcionam com a janela focada. O sidebar precisa de
atalho global de verdade: `tauri-plugin-global-shortcut` (dependência pequena).

### 3.3 Alt+W → Alt+A: possível, com um custo

Um acorde de **duas teclas cruas** (W depois A) exigiria hook de teclado global de baixo nível
(`WH_KEYBOARD_LL`) — roda em todo keystroke do sistema e é o padrão que antivírus classifica
como keylogger. Descartado.

Já **dois aceleradores em sequência** (`Alt+W` depois `Alt+A`) é perfeitamente registrável, e
foi o que se pediu:

1. `Alt+W` fica registrado permanentemente como prefixo.
2. Ao ser pressionado, registra-se `Alt+A` e arma-se uma janela de ~1,2 s.
3. `Alt+A` dentro da janela → alterna o sidebar; depois disso, `Alt+A` é liberado de volta.

Custo a aceitar: **o prefixo fica reservado ao WhatsApp Lite globalmente** e para de funcionar
nos outros apps (não dá para "replayar" a tecla engolida para a janela em foco). O segundo
acelerador só é capturado durante a janela de 1,2 s, então esse não atrapalha ninguém.

A config deve oferecer os dois modos — atalho único ou acorde — ambos personalizáveis, e a UI
precisa avisar sobre o efeito do prefixo. Conflito com outro app aparece como erro no
`register()` e deve ser mostrado na UI, mantendo o atalho anterior.

### 3.4 Overlay, não AppBar

Duas arquiteturas possíveis:

- **Overlay flutuante** — janela sem borda, always-on-top, colada na borda, por cima das outras
  janelas. Simples, funciona em Windows e Linux. **Escolhido.**
- **AppBar nativa** (`SHAppBarMessage`/`ABM_SETPOS`) — reserva espaço na work area, janelas
  maximizadas encolhem e o sidebar nunca cobre nada. Muito mais caro, só Windows, e se o app
  morrer sem mandar `ABM_REMOVE` a área de trabalho fica com um buraco até relogar. Descartado
  por ora; fica como possível evolução.

Mesmo sendo overlay, a geometria deve respeitar a **work area** do monitor (não a tela cheia),
para não cobrir a barra de tarefas.

### 3.5 O risco número um: largura

`min_inner_size` hoje é 780px (`lib.rs:1763`) — sozinho isso já bloqueia um sidebar de 400px e
precisa ser ajustado por modo. O risco de verdade é outro: **o WhatsApp Web pode ficar
inutilizável numa coluna estreita.**

O WhatsApp Android colapsa lista e conversa em painel único quando não há espaço. Duas saídas
possíveis, a escolher **depois do teste de largura**:

- **(A) Zoom-out da webview** — `ICoreWebView2Controller::SetZoomFactor` (~0.85) no Windows,
  `webkit_web_view_set_zoom_level` no Linux. Barato e robusto; o padrão `with_webview` já
  existe no projeto em `apply_low_power_mode` (`lib.rs:1790`). Ganha largura efetiva sem tocar
  no DOM do WhatsApp.
- **(B) CSS de painel único** — injetar CSS só em modo sidebar: esconder `#pane-side` quando há
  conversa aberta e oferecer um "voltar" que dispara Escape (mesmo caminho do Ctrl+W já
  implementado em `lib.rs:1199`). Emula o Android de fato, mas depende do DOM do WhatsApp Web,
  que muda sem aviso. `#pane-side` é dos seletores mais estáveis que eles têm.

Recomendação: **(A) como base, (B) como refinamento** se (A) não bastar.

> **Pendente:** medir na mão — abrir o app, arrastar até ~400px e ver o que quebra.
> O resultado desse teste decide 3.5 e a largura padrão.

### 3.6 Detalhes que vão morder

- **Auto-hide** ao perder foco precisa de exceções, senão o sidebar some na hora errada:
  durante o diálogo nativo de salvar (`open_save_dialog`, `lib.rs:109` — rouba foco), durante
  chamadas (`call_active`, `lib.rs:216` já existe), com o "fixar" ligado, e quando o foco vai
  para a janela de configurações. Vale um debounce de ~150–200 ms para não esconder num piscar
  de foco logo depois de mostrar.
- **Flapping do low-power**: `apply_low_power_mode` dispara na transição escondido↔visível. Com
  auto-hide, o toggle fica frequente e vira trim/restore de memória repetido, deixando a
  próxima abertura mais lenta. Corrigir com um atraso (só entra em baixo consumo depois de
  ~60 s escondido).
- **`set_decorations(false)` em runtime** no Windows com WebView2 tem histórico de quirks
  (sombra, cantos, borda de resize). Fallback, se der problema: janela sempre sem decoração +
  barra de título própria desenhada no modo normal. Recriar a janela **não** é opção — custaria
  um reload completo do WhatsApp Web e mataria a proposta de abertura instantânea.
- **`min_inner_size`** precisa ser afrouxado ao entrar no modo sidebar e restaurado ao sair.
- **Multi-monitor e DPI**: calcular em pixels físicos a partir do retângulo do monitor alvo,
  aplicando `scale_factor`. Monitor alvo conforme a preferência do usuário (principal ou o do
  cursor, via `cursor_position()` + `monitor_from_point`, com fallback no primário).
- **Work area**: preferir `Monitor::work_area()` se disponível na versão de Tauri em uso; caso
  contrário, `MonitorFromPoint` + `GetMonitorInfoW` (`rcWork`) atrás de `#[cfg(windows)]` — isso
  exige a feature `Win32_Graphics_Gdi` no crate `windows`. No Linux, cair para o tamanho do
  monitor.
- **Geometria do modo normal** precisa ser salva ao entrar no sidebar e restaurada ao sair.
- **`focus_main_window`** (`lib.rs:244`) precisa passar a respeitar o modo: clique em
  notificação com sidebar ativo deve reabrir como sidebar, não como janela normal.
- **Linux**: atalho global funciona no X11 e é problemático no Wayland — documentar a limitação.

### 3.7 Restrições do projeto a respeitar

- `scripts/verify-no-right-click.js` varre `src/` e `src-tauri/src/` e reprova certos termos e
  padrões de interação de clique secundário. Código e comentários novos precisam evitá-los.
- `scripts/verify-no-download-banner.js` lê `src-tauri/src/lib.rs` e **proíbe
  `setInterval`/`requestAnimationFrame` no arquivo inteiro**. Qualquer JS novo injetado deve
  usar cadeias de `setTimeout`.
- Ambos rodam na CI (`.github/workflows/linux.yml`), junto de `cargo test`.

## 4. Decisões já tomadas

| Tema | Decisão |
| --- | --- |
| Arquitetura | Overlay flutuante, uma janela em dois modos |
| Lado | Configurável; padrão à direita |
| Atalho | Suporte a atalho único **e** a acorde de dois aceleradores, ambos personalizáveis |
| Auto-hide | Sim, ao clicar fora |
| Multi-monitor | Configurável: monitor principal **ou** o do cursor |
| Configurações | Terá interface própria, com botão próprio |
| Animação | Fora da v1 — toggle instantâneo primeiro, medir, animar depois |
| Limite de RAM | Descartado (custo marginal ≈ 0) |

## 5. Perguntas em aberto

1. **Onde vive a UI de configuração?**
   (a) Janela Tauri própria — `settings.html` em `src/`, aberta sob demanda e destruída ao
   fechar; HTML/CSS/JS puro, sem build step; ~50–80 MB só enquanto aberta.
   (b) Painel injetado na página do WhatsApp Web pelo patch JS — zero RAM extra, mas sujeito ao
   DOM/CSS do WhatsApp e precisa ser reinjetado a cada reload automático.

2. **O sidebar sem decoração não tem onde pôr botões nossos (a webview ocupa 100%). Entra uma
   barra fina de ~28px injetada no topo, só em modo sidebar?** Ela carregaria: expandir para
   janela normal, abrir config, fixar (desliga o auto-hide temporariamente) e alça para arrastar
   a largura. Sem ela, sair do modo e ajustar largura só pelo tray e por atalhos.

3. **Prefixo do acorde.** `Alt+W` como padrão (fica reservado globalmente, some dos outros apps)
   ou um padrão mais raro (`Ctrl+Alt+W`), deixando `Alt+W` disponível na config?

4. **Resultado do teste de largura** — o WhatsApp Web aguenta ~400px? Decide entre zoom-out (A),
   CSS de painel único (B), ou uma largura padrão maior.

## 6. Plano de implementação sugerido

Ordem pensada para cada fase ser testável sozinha. Estimativa total: ~700 linhas.

**Fase 0 — Configuração e persistência.**
`Settings` com `serde` (já é dependência), salvo em `app_config_dir()/settings.json` com
escrita atômica (arquivo temporário + `rename`). Campos: `sidebar_enabled`, `side`, `width`,
`monitor`, `autohide`, `hotkey_mode`, `hotkey`, `hotkey_second`, `chord_timeout_ms`,
`normal_geometry`. `#[serde(default)]` para tolerar arquivo antigo. Guardado em `RuntimeState`
(`lib.rs:53`) atrás de um `Mutex`. Com `sidebar_enabled: false`, o comportamento atual do app
fica 100% preservado.

**Fase 1 — Geometria e troca de modo (núcleo).**
`enter_sidebar_mode` / `exit_sidebar_mode`: salvar e restaurar geometria normal, alternar
`decorations` / `always_on_top` / `skip_taskbar`, afrouxar e restaurar `min_inner_size`,
posicionar em pixels físicos na work area do monitor alvo. Função pura
`sidebar_rect(work_area, side, width, scale)` — testável sem janela.

**Fase 2 — Atalho global e acorde.**
`tauri-plugin-global-shortcut`. Máquina de estados do acorde como função pura (`Idle` /
`Armed { until }`), com contador de geração para evitar corrida entre o timer de expiração e um
novo disparo. Registro do segundo acelerador só enquanto armado. Erro de registro devolvido à
UI de config.

**Fase 3 — Auto-hide e integração com o que já existe.**
`WindowEvent::Focused(false)` com debounce; predicado puro
`should_autohide(modo, autohide, fixado, chamada_ativa, dialogo_aberto, config_aberta)`.
Marcar `dialog_open` em volta de `open_save_dialog` (`lib.rs:109`). Atrasar a entrada em baixo
consumo (~60 s) em `start_webview_maintenance` (`lib.rs:1935`). Roteirizar `focus_main_window`
(`lib.rs:244`) pelo modo atual.

**Fase 4 — Tray e chrome do sidebar.**
Itens "Modo sidebar" (check) e "Configurações" no menu do tray. Se a barra fina for aprovada
(pergunta 2): injetada pelo patch JS sob um flag setado do Rust, com arrasto e redimensionamento
via comandos próprios (`start_dragging` / `start_resize_dragging`), e largura persistida com
debounce no `WindowEvent::Resized`. Sem `setInterval` (ver 3.7).

**Fase 5 — Janela de configurações.**
Conforme a resposta da pergunta 1. Se for janela própria: `src/settings.html` + `settings.js`,
`WebviewUrl::App`, destruída ao fechar, e `"settings"` adicionado ao array `windows` de
`src-tauri/capabilities/default.json` (hoje só `["main"]`).

**Fase 6 — Layout estreito.**
Conforme o teste de largura: zoom-out (A) e/ou CSS de painel único (B).

**Fase 7 — Testes.**
No módulo `#[cfg(test)]` que já existe em `lib.rs`: `sidebar_rect` (direita/esquerda, DPI 150%,
clamp), serde de `Settings` (default, ida e volta, campo desconhecido), matriz de
`should_autohide`, expiração do acorde, decisão de alternar (visível+focado → esconde;
visível sem foco → foca).

### Arquivos afetados

- `src-tauri/Cargo.toml` — `tauri-plugin-global-shortcut`; feature `Win32_Graphics_Gdi` se o
  fallback de work area for necessário.
- `src-tauri/src/lib.rs` — fiação: estado, tray, eventos de janela, comandos.
- `src-tauri/src/sidebar.rs` e `src-tauri/src/settings.rs` (novos) — `lib.rs` já tem 2.137
  linhas; separar evita levá-lo a ~2.800. Desvio consciente do "tudo em lib.rs" atual.
- `src/settings.html`, `src/settings.js` (novos, se a UI for janela própria).
- `src-tauri/capabilities/default.json` — incluir a janela `settings`.
- `README.md` — documentar o recurso e o atalho.

### Verificação

- `cargo test --manifest-path src-tauri/Cargo.toml`
- `npm run verify:no-right-click` e `npm run verify:no-download-banner`
- `npm run dev` e checar na mão: dois monitores, um deles a 150% de escala; acorde e atalho
  único; auto-hide não disparando durante download e durante chamada; barra de tarefas não
  coberta; geometria normal restaurada ao sair do modo; RAM no Gerenciador de Tarefas antes e
  depois de vários toggles (não deve subir).

## 7. Achado colateral: CI vermelha no `main`

`npm run verify:no-download-banner` **já falha no `main`**, por três motivos, todos consequência
do commit `6833b12` ("Improve performance: adaptive reload, lighter DOM observer…") ter melhorado
o observer sem atualizar o script:

- `missing: sync mutation observer` — o script exige `new MutationObserver(() => {`, mas o
  código agora usa `new MutationObserver((mutations) => {` com pré-filtro.
- `missing: text mutations observed` — o script exige `characterData: true`, que o observer novo
  não usa mais.
- `forbidden: paint-delayed banner removal` — o script proíbe `setInterval|requestAnimationFrame`
  no `lib.rs` inteiro, e hoje existem dois `setInterval` legítimos (reporte de heap e refresh de
  Não Perturbe).

Conserto sugerido, em commit separado: atualizar o script para o observer novo e restringir a
proibição de temporizadores ao trecho do banner, em vez do arquivo inteiro. Enquanto isso, todo
PR nasce vermelho por um motivo que não tem nada a ver com o PR.
