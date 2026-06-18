# WhatsApp Lite (Rust + Tauri)

WhatsApp Lite e um cliente desktop ultra-leve para Windows e Linux baseado em Rust + Tauri.
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
- DEB (pacote Linux)
- AppImage (Linux portatil)

## Estrutura principal

- [src](src): splash minimo de inicializacao.
- [src-tauri/src/lib.rs](src-tauri/src/lib.rs): backend Rust + comandos + patch JS.
- [src-tauri/tauri.conf.json](src-tauri/tauri.conf.json): configuracao do app e bundle.
- [scripts/verify-no-right-click.js](scripts/verify-no-right-click.js): verificacao de politica do projeto.

## Requisitos de build

- Node.js LTS
- Rust toolchain
- Dependencias de build do Tauri para Windows (MSVC/Build Tools)

No Ubuntu/Debian:

```bash
sudo apt update
sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev
```

## Executar em desenvolvimento

```bash
npm install
npm run dev
```

## Gerar build e instalador

```bash
npm run build
```

Saida padrao no Windows:

- src-tauri/target/release/bundle/nsis/

Saida padrao no Linux:

- src-tauri/target/release/bundle/deb/
- src-tauri/target/release/bundle/appimage/

Para Arch Linux, use o arquivo `.AppImage`, nao o `.deb`:

```bash
chmod +x WhatsAppLite*.AppImage
./WhatsAppLite*.AppImage
```

No Windows, o Tauri usa Edge WebView2. No Linux, usa WebKitGTK. A casca Rust/Tauri continua leve, mas o desempenho real do WhatsApp Web precisa ser testado no Linux.

## Verificar downloads

Releases publicam:

- `WhatsAppLite.exe`
- `WhatsAppLite_*_x64-setup.exe`
- `WhatsAppLite_*_amd64.deb`
- `WhatsAppLite_*_amd64.AppImage`
- `SHA256SUMS.txt`

Para conferir se o arquivo baixado bate com o release:

```powershell
Get-FileHash .\WhatsAppLite.exe -Algorithm SHA256
Get-FileHash .\WhatsAppLite_1.0.2_x64-setup.exe -Algorithm SHA256
```

No Linux:

```bash
sha256sum WhatsAppLite_*.deb WhatsAppLite_*.AppImage
```

Compare o hash com o `SHA256SUMS.txt` do mesmo release.

## Transparencia e seguranca

O app e um wrapper Tauri para `https://web.whatsapp.com`. Ele nao tem servidor proprio, nao coleta login e nao envia mensagens para terceiros.

O codigo injeta um patch local para integrar notificacoes, downloads, atalhos, tray e abertura segura de links externos. Veja [SECURITY.md](SECURITY.md) para detalhes.

O Windows SmartScreen pode alertar porque este e um app novo/independente. Confira o repositorio, os hashes do release e compile localmente se preferir.

## Transparency and security

This app is a Tauri wrapper for `https://web.whatsapp.com`. It has no backend server, does not collect logins, and does not send messages to third parties.

It injects a local patch to integrate native notifications, downloads, shortcuts, tray behavior, and safe external-link handling. See [SECURITY.md](SECURITY.md) for details.

Windows SmartScreen may warn because this is a new independent app. Check the repository, verify the release hashes, or build it locally if you prefer.

## Aviso legal

Este projeto e independente e nao possui afiliacao oficial com WhatsApp ou Meta.
WhatsApp e marca de seus respectivos proprietarios.

## Disclaimer

This project is independent and is not officially affiliated with WhatsApp or Meta.
WhatsApp is a trademark of its respective owners.
