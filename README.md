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

## Verificar downloads

Releases publicam:

- `WhatsAppLite.exe`
- `WhatsAppLite_*_x64-setup.exe`
- `SHA256SUMS.txt`

Para conferir se o arquivo baixado bate com o release:

```powershell
Get-FileHash .\WhatsAppLite.exe -Algorithm SHA256
Get-FileHash .\WhatsAppLite_1.0.2_x64-setup.exe -Algorithm SHA256
```

Compare o hash com o `SHA256SUMS.txt` do mesmo release.

## Transparencia e seguranca

O app e um wrapper Tauri para `https://web.whatsapp.com`. Ele nao tem servidor proprio, nao coleta login e nao envia mensagens para terceiros.

O codigo injeta um patch local para integrar notificacoes, downloads, atalhos, tray e abertura segura de links externos. Veja [SECURITY.md](SECURITY.md) para detalhes.

O Windows SmartScreen pode alertar porque este e um app novo/independente. Confira o repositorio, os hashes do release e compile localmente se preferir.

## Aviso legal

Este projeto e independente e nao possui afiliacao oficial com WhatsApp ou Meta.
WhatsApp e marca de seus respectivos proprietarios.
