// Diálogo de "Adicionar perfil".
//
// HTML/CSS/JS puro, sem build step — o projeto não tem bundler e não vale
// adicionar um por causa de um formulário de dois campos.

const invoke = (cmd, args) => window.__TAURI_INTERNALS__.invoke(cmd, args || {});

const $ = (id) => document.getElementById(id);

// Na primeira vez o usuário batiza também a sessão que já está aberta; nas
// próximas só o perfil novo.
let needsCurrentName = false;

function showError(text) {
  const status = $('status');
  status.textContent = text;
  status.hidden = false;
}

async function init() {
  try {
    const info = await invoke('profiles_info');
    needsCurrentName = info.needs_current_name;
    $('current_field').hidden = !needsCurrentName;
  } catch (err) {
    showError(String(err));
  }
  (needsCurrentName ? $('current_name') : $('new_name')).focus();
}

async function submit(event) {
  event.preventDefault();
  $('status').hidden = true;
  $('submit').disabled = true;

  try {
    await invoke('profiles_add', {
      currentName: needsCurrentName ? $('current_name').value : null,
      newName: $('new_name').value,
    });
    // Deu certo: o app reinicia sozinho no perfil novo, esta janela morre junto.
  } catch (err) {
    showError(String(err));
    $('submit').disabled = false;
  }
}

$('form').addEventListener('submit', submit);
$('cancel').addEventListener('click', () => invoke('profiles_cancel'));
document.addEventListener('keydown', (event) => {
  if (event.key === 'Escape') invoke('profiles_cancel');
});

init();
