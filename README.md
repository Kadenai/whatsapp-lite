# WhatsApp Lite (Rust + Tauri)

WhatsApp Lite e um cliente desktop ultra-leve para Windows baseado em Rust + Tauri.
Ele abre o WhatsApp Web dentro de um WebView, mas adiciona comportamento nativo de desktop para focar em desempenho.

Este projeto nasceu da insatisfacao com a versao atual do WhatsApp Desktop, que é muito lento e instável.

## Como o aplicativo funciona

Arquitetura resumida:

1. Frontend minimo em [src/index.html](src/index.html), [src/main.js](src/main.js) e [src/styles.css](src/styles.css).
2. Janela principal Tauri carrega https://web.whatsapp.com.
3. Backend Rust injeta um patch JavaScript para adicionar recursos nativos e atalhos.
4. Plugins Tauri fazem integracao com dialogo de arquivo, sistema de notificacao, autostart e tray.

## Recursos implementados

- Instancia unica: se abrir de novo, foca a janela existente.
- Tray icon com menu (abrir/focar, autostart e sair).
- Autostart com Windows (inicializacao com --hidden).
- Notificacoes nativas de nova mensagem (com foco/abertura da janela ao clicar no popup no Windows).
- Regra para notificar somente quando a janela nao esta ativa.
- Ignora notificacao de chats silenciados.
- Atalhos extras:
	- Ctrl+W para fechar conversa (simulando Escape).
	- Ctrl+Seta para cima para editar ultima mensagem enviada.
- Download com dialogo nativo de salvar arquivo.
- Abertura segura de links externos via sistema operacional.

## Tecnologias

- Rust
- Tauri v2
- JavaScript (patch injetado)
- NSIS (instalador Windows)

## Estrutura principal

- [src](src): splash minimo de inicializacao.
- [src-tauri/src/lib.rs](src-tauri/src/lib.rs): backend Rust + comandos + patch JS.
- [src-tauri/tauri.conf.json](src-tauri/tauri.conf.json): configuracao do app e bundle.
- [scripts/verify-no-right-click.js](scripts/verify-no-right-click.js): verificacao de politica do projeto.

## Requisitos de build

- Node.js LTS
- Rust toolchain
- Dependencias de build do Tauri para Windows (MSVC/Build Tools)

## Executar em desenvolvimento

```bash
npm install
npm run dev
```

## Gerar build e instalador

```bash
npm run build
```

Saida padrao do instalador NSIS:

- src-tauri/target/release/bundle/nsis/

## Aviso legal

Este projeto e independente e nao possui afiliacao oficial com WhatsApp ou Meta.
WhatsApp e marca de seus respectivos proprietarios.