// The demo padlock: send the password; on success the shackle lifts and the app loads.
const form = document.getElementById('gate');
const input = document.getElementById('pw');
const msg = document.getElementById('msg');
const button = form.querySelector('button');
const gate = document.querySelector('.gate');
const reduce = matchMedia('(prefers-reduced-motion: reduce)').matches;

form.addEventListener('submit', async (event) => {
  event.preventDefault();
  msg.textContent = '';
  if (!input.value) { input.focus(); return; }
  button.disabled = true;
  try {
    const r = await fetch('/api/gate', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ password: input.value }),
      credentials: 'same-origin',
    });
    if (r.status === 204) {
      gate.classList.add('open');
      setTimeout(() => gate.classList.add('gone'), reduce ? 0 : 650);
      setTimeout(() => location.replace(location.pathname + location.search), reduce ? 60 : 1100);
      return;
    }
    let text = 'That’s not the password.';
    try { const j = await r.json(); if (j && j.error && j.error.message) text = j.error.message; } catch { /* keep the default */ }
    msg.textContent = text;
    form.classList.remove('shake'); void form.offsetWidth; form.classList.add('shake');
    input.select();
  } catch {
    msg.textContent = 'Couldn’t reach the server. Check your connection and try again.';
  }
  if (!gate.classList.contains('open')) button.disabled = false;
});
