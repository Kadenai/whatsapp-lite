// Tela de boas-vindas da primeira execucao: pergunta se o computador e
// compartilhado e, em caso positivo, cria o primeiro perfil.

const invoke = (cmd, args) => window.__TAURI_INTERNALS__.invoke(cmd, args || {});

const $ = (id) => document.getElementById(id);

const STEPS = ['step_ask', 'step_name', 'step_alone'];

function show(step) {
  for (const id of STEPS) $(id).hidden = id !== step;
}

function showError(text) {
  const status = $('status');
  status.textContent = text;
  status.hidden = false;
}

$('shared').addEventListener('click', () => {
  show('step_name');
  $('name').focus();
});

$('back').addEventListener('click', () => show('step_ask'));

// "So eu uso": grava a resposta antes de mostrar o aviso, pra a pergunta nao
// voltar mesmo que a janela seja fechada no X em vez do botao.
$('alone').addEventListener('click', async () => {
  try {
    await invoke('onboarding_dismiss');
  } catch (err) {
    showError(String(err));
  }
  show('step_alone');
});

$('done').addEventListener('click', () => invoke('onboarding_close'));

$('form').addEventListener('submit', async (event) => {
  event.preventDefault();
  $('status').hidden = true;
  $('submit').disabled = true;

  try {
    await invoke('onboarding_create', { name: $('name').value });
    // Deu certo: o app reinicia dentro do perfil novo e esta janela morre junto.
  } catch (err) {
    showError(String(err));
    $('submit').disabled = false;
  }
});
