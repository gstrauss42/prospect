// The hidden feature-flag page: authenticate with the admin password, then switch the padlock.
const auth = document.getElementById('auth');
const input = document.getElementById('pw');
const flags = document.getElementById('flags');
const status = document.getElementById('status');
const note = document.getElementById('note');
const msg = document.getElementById('msg');
const on = document.getElementById('on');
const off = document.getElementById('off');

async function call(body) {
  const r = await fetch('/__admin/flags', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ password: input.value, ...body }),
    credentials: 'same-origin',
  });
  const j = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error((j.error && j.error.message) || 'Something went wrong. Try again.');
  return j;
}

function show(state) {
  status.textContent = state.padlock ? 'on' : 'off';
  note.textContent = !state.password_set
    ? 'DEMO_PASSWORD isn’t set, so there’s no padlock to show even when this is on.'
    : state.padlock ? 'Visitors need the demo password to get in.' : 'Anyone can open the site.';
  on.disabled = state.padlock;
  off.disabled = !state.padlock;
  flags.hidden = false;
}

auth.addEventListener('submit', async (event) => {
  event.preventDefault();
  msg.textContent = ''; msg.classList.remove('ok');
  try { show(await call({})); auth.hidden = true; }
  catch (err) { msg.textContent = err.message; input.select(); }
});

for (const [button, value] of [[on, true], [off, false]]) {
  button.addEventListener('click', async () => {
    msg.textContent = ''; msg.classList.remove('ok');
    try {
      show(await call({ padlock: value }));
      msg.textContent = value ? 'Padlock is on.' : 'Padlock is off.';
      msg.classList.add('ok');
    } catch (err) { msg.textContent = err.message; }
  });
}
