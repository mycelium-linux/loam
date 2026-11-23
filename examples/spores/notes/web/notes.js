(() => {
  const list = document.getElementById('note-list');
  const count = document.getElementById('count');
  const status = document.getElementById('status');
  const title = document.getElementById('note-title');
  const body = document.getElementById('note-body');
  const meta = document.getElementById('note-meta');
  const empty = document.getElementById('note-empty');
  const save = document.getElementById('save-note');
  let selected = null;
  let version = null;
  let dirty = false;

  const desktop = new URLSearchParams(location.search).get('desktop') || 'personal';
  const withDesktop = (path) => `${path}${path.includes('?') ? '&' : '?'}desktop=${encodeURIComponent(desktop)}`;
  const api = async (path, options = {}) => {
    const response = await fetch(withDesktop(path), options);
    const value = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(value.error || `${response.status} ${response.statusText}`);
    return value;
  };

  function setStatus(text, error = false) {
    status.textContent = text;
    status.className = error ? 'note-error' : '';
  }

  function markDirty(value) {
    dirty = value;
    if (value) setStatus('Unsaved changes');
  }

  function render(entries) {
    const notes = entries
      .filter((entry) => entry.media_type.startsWith('text/markdown') || entry.path.endsWith('.md'))
      .sort((a, b) => (b.updated_at_ms || 0) - (a.updated_at_ms || 0));
    list.replaceChildren();
    count.textContent = `${notes.length} ${notes.length === 1 ? 'note' : 'notes'}`;
    if (!notes.length) {
      const quiet = document.createElement('p');
      quiet.className = 'sidebar-foot';
      quiet.textContent = 'No Markdown yet.';
      list.append(quiet);
      return;
    }
    for (const note of notes) {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'note-row';
      button.dataset.path = note.relative;
      button.setAttribute('aria-current', String(selected === note.relative));
      const name = document.createElement('span');
      name.textContent = note.relative.replace(/\.md$/i, '').split('/').pop();
      const stamp = document.createElement('small');
      stamp.textContent = note.updated_at_ms ? new Date(note.updated_at_ms).toLocaleDateString() : '';
      button.append(name, stamp);
      button.addEventListener('click', () => openNote(note.relative));
      list.append(button);
    }
  }

  async function refresh() {
    try {
      const result = await api('/api/vfs/list?path=');
      render(result.entries);
      setStatus('Workspace ready');
      const first = list.querySelector('.note-row');
      if (!selected && first) await openNote(first.dataset.path);
    } catch (error) {
      setStatus(error.message, true);
    }
  }

  async function openNote(path) {
    if (dirty && !confirm('Discard unsaved changes?')) return;
    try {
      const note = await api(`/api/vfs/read?path=${encodeURIComponent(path)}`);
      selected = path;
      version = note.version ?? null;
      title.value = path.split('/').pop().replace(/\.md$/i, '');
      body.value = note.text;
      body.disabled = false;
      empty.hidden = true;
      meta.textContent = version == null ? '' : `v${version}`;
      markDirty(false);
      setStatus('Open');
      window.__MYCELIUM_AGENT_CONTEXT = { app: 'Notes', detail: path };
      window.dispatchEvent(new CustomEvent('mycelium-agent-context', { detail: window.__MYCELIUM_AGENT_CONTEXT }));
      for (const row of list.querySelectorAll('.note-row')) row.setAttribute('aria-current', String(row.dataset.path === path));
    } catch (error) {
      setStatus(error.message, true);
    }
  }

  async function saveNote() {
    if (!selected) return;
    try {
      const path = selected;
      const expected = version == null ? '' : `&expected_version=${version}`;
      const result = await api(`/api/vfs/write?path=${encodeURIComponent(path)}${expected}`, {
        method: 'PUT',
        headers: { 'content-type': 'text/markdown; charset=utf-8' },
        body: body.value,
      });
      version = result.version;
      meta.textContent = `v${version}`;
      markDirty(false);
      setStatus('Saved');
      await refresh();
    } catch (error) {
      setStatus(error.message, true);
    }
  }

  document.getElementById('new-note').addEventListener('click', async () => {
    if (dirty && !confirm('Discard unsaved changes?')) return;
    const name = prompt('New note name', 'untitled.md');
    if (!name) return;
    const clean = name.trim().replace(/[^a-zA-Z0-9._/-]+/g, '-').replace(/^\/+/, '');
    if (!clean) return;
    const path = clean.endsWith('.md') ? clean : `${clean}.md`;
    selected = path;
    version = null;
    title.value = path.replace(/\.md$/i, '');
    body.value = '';
    body.disabled = false;
    empty.hidden = true;
    meta.textContent = 'new';
    markDirty(true);
    setStatus('New note — save when ready');
  });

  save.addEventListener('click', saveNote);
  body.addEventListener('input', () => markDirty(true));
  title.addEventListener('input', () => markDirty(true));
  document.addEventListener('keydown', (event) => {
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 's') {
      event.preventDefault();
      saveNote();
    }
  });
  window.addEventListener('beforeunload', (event) => {
    if (dirty) event.preventDefault();
  });

  refresh();
})();
