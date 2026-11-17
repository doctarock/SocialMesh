// A one-vote-per-person poll, as a SocialMesh app (SPEC.md §11).
//
// This runs in a sandboxed worker with no network. It reads the votes sent
// so far (entries), draws the poll with socialmesh.render, and asks to send
// a vote when a button is clicked. Each person's latest vote counts once.

const OPTIONS = [
  ['yes', 'Yes'],
  ['no', 'No'],
  ['unsure', 'Not sure'],
];

function label(key) {
  const o = OPTIONS.find((o) => o[0] === key);
  return o ? o[1] : null;
}

async function show() {
  const votes = new Map(); // voter -> their latest vote
  let mine = null;
  for (const e of await socialmesh.entries()) {
    let vote;
    try {
      vote = JSON.parse(e.data).vote;
    } catch (_) {
      continue;
    }
    if (!label(vote)) continue;
    votes.set(e.author || e.id, vote);
    if (e.mine) mine = vote;
  }
  const counts = {};
  for (const v of votes.values()) counts[v] = (counts[v] || 0) + 1;
  const total = votes.size;

  let html = '<div class="options">';
  for (const [key, text] of OPTIONS) {
    const n = counts[key] || 0;
    const pct = total ? Math.round((100 * n) / total) : 0;
    html +=
      `<button data-action="vote" data-value="${key}" class="${mine === key ? 'chosen' : ''}">` +
      `<span class="bar" style="width:${pct}%"></span><span>${text}</span><b>${n}</b></button>`;
  }
  html += '</div>';
  const you = mine ? ` · you voted ${label(mine)} (tap another to change it)` : '';
  html += `<p class="note">${total} vote${total === 1 ? '' : 's'} so far${you}.</p>`;
  socialmesh.render(html);
}

socialmesh.on('start', show);

socialmesh.on('click', async (e) => {
  if (e.action !== 'vote' || !label(e.value)) return;
  try {
    await socialmesh.submit({ vote: e.value }, `Vote "${label(e.value)}"`);
  } catch (_) {
    // They chose not to send it.
  }
  show();
});
