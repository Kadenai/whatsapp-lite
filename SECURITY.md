# Segurança

## O que este app faz

WhatsApp Lite abre `https://web.whatsapp.com` em uma janela Tauri e adiciona integracoes locais de desktop:

- notificacoes nativas;
- dialogo nativo para salvar downloads;
- atalhos de teclado;
- bandeja do sistema;
- abertura de links externos no navegador padrao;
- remocao/substituicao do banner de download do WhatsApp Desktop.

## O que este app nao faz

- Nao tem servidor proprio.
- Nao coleta login.
- Nao envia mensagens, contatos, midia ou cookies para terceiros.
- Nao substitui a criptografia ou a conta do WhatsApp.

## Por que existe codigo injetado

O WhatsApp Web roda dentro de um WebView. O app injeta um patch local para adaptar APIs do navegador, como `Notification`, downloads e links externos, para recursos nativos do Windows.

Esse patch fica em `src-tauri/src/lib.rs`.

## Como verificar um release

Baixe os artefatos e o `SHA256SUMS.txt` do mesmo release.

No PowerShell:

```powershell
Get-FileHash .\WhatsAppLite.exe -Algorithm SHA256
Get-FileHash .\WhatsAppLite_1.0.2_x64-setup.exe -Algorithm SHA256
```

Compare os hashes com o arquivo `SHA256SUMS.txt`.

## SmartScreen

O Windows SmartScreen pode mostrar aviso para apps novos ou pouco baixados, mesmo quando o arquivo e legitimo. Isso e reputacao do Windows, nao prova de malware.

Antes de instalar, confira:

- o repositorio de origem;
- os hashes do release;
- o historico de commits;
- o workflow de release em `.github/workflows/release.yml`.

## Assinatura de codigo

Os binarios ainda nao sao assinados com certificado de publicador. Quando houver certificado, o instalador e o executavel devem ser assinados no workflow de release.

# Security

## What this app does

WhatsApp Lite opens `https://web.whatsapp.com` in a Tauri window and adds local desktop integrations:

- native notifications;
- native save dialog for downloads;
- keyboard shortcuts;
- system tray;
- external links opened in the default browser;
- removal/replacement of the WhatsApp Desktop download banner.

## What this app does not do

- It has no backend server.
- It does not collect logins.
- It does not send messages, contacts, media, or cookies to third parties.
- It does not replace WhatsApp encryption or your WhatsApp account.

## Why injected code exists

WhatsApp Web runs inside a WebView. The app injects a local patch to adapt browser APIs, such as `Notification`, downloads, and external links, to native Windows behavior.

That patch lives in `src-tauri/src/lib.rs`.

## How to verify a release

Download the artifacts and `SHA256SUMS.txt` from the same release.

In PowerShell:

```powershell
Get-FileHash .\WhatsAppLite.exe -Algorithm SHA256
Get-FileHash .\WhatsAppLite_1.0.2_x64-setup.exe -Algorithm SHA256
```

Compare the hashes with `SHA256SUMS.txt`.

## SmartScreen

Windows SmartScreen may show a warning for new or rarely downloaded apps, even when the file is legitimate. This is Windows reputation, not proof of malware.

Before installing, check:

- the source repository;
- the release hashes;
- the commit history;
- the release workflow at `.github/workflows/release.yml`.

## Code signing

The binaries are not signed with a publisher certificate yet. Once a certificate is available, the installer and executable should be signed in the release workflow.
