(() => {
  const list = document.getElementById('contact-list');
  const count = document.getElementById('count');
  const status = document.getElementById('status');
  const empty = document.getElementById('empty');
  const form = document.getElementById('form');
  const reload = document.getElementById('reload-contacts');
  const fields = {
    name: document.getElementById('field-name'),
    email: document.getElementById('field-email'),
    phone: document.getElementById('field-phone'),
    role: document.getElementById('field-role'),
    note: document.getElementById('field-note'),
  };
  let book = { contacts: [] };
  let version = null;
  let selected = null;
  let dirty = false;
  let saving = false;
  let queued = false;
  let conflict = false;
  let saveTimer = 0;

  const desktop = new URLSearchParams(location.search).get('desktop') || 'personal';
  const withDesktop = (path) => `${path}${path.includes('?') ? '&' : '?'}desktop=${encodeURIComponent(desktop)}`;
  const api = async (path, options = {}) => {
    const response = await fetch(withDesktop(path), options);
    const value = await response.json().catch(() => ({}));
    if (!response.ok) {
      const error = new Error(value.error || `${response.status} ${response.statusText}`);
      error.status = response.status;
      throw error;
    }
    return value;
  };
  const setStatus = (text, error = false) => {
    status.textContent = text;
    status.className = error ? 'contact-error' : '';
  };
  const setConflict = (value) => {
    conflict = value;
    if (reload) reload.hidden = !value;
  };
  const markDirty = (value) => {
    dirty = value;
    if (value && !conflict) setStatus('Editing…');
  };
  const initials = (name) => (name || '?').trim().split(/\s+/).map((part) => part[0]).join('').slice(0, 2).toUpperCase();

  function scheduleSave(delay = 700) {
    if (conflict) return;
    clearTimeout(saveTimer);
    saveTimer = window.setTimeout(() => void saveBook(), delay);
  }

  function row(contact) {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'contact-row';
    button.setAttribute('aria-current', String(selected === contact.id));
    const avatar = document.createElement('span');
    avatar.className = 'contact-avatar';
    avatar.textContent = initials(contact.name);
    const text = document.createElement('span');
    const name = document.createElement('strong');
    name.textContent = contact.name || 'Unnamed';
    const sub = document.createElement('small');
    sub.textContent = contact.role || contact.email || 'Contact';
    text.append(name, sub);
    button.append(avatar, text);
    button.addEventListener('click', () => void select(contact.id));
    return button;
  }

  function render() {
    list.replaceChildren();
    const contacts = book.contacts || [];
    count.textContent = `${contacts.length} ${contacts.length === 1 ? 'contact' : 'contacts'}`;
    if (!contacts.length) {
      const quiet = document.createElement('p');
      quiet.className = 'contact-empty';
      quiet.textContent = 'No contacts yet.';
      list.append(quiet);
      return;
    }
    for (const contact of contacts) list.append(row(contact));
  }

  async function select(id) {
    if (dirty && !conflict) await saveBook();
    if (conflict) {
      setStatus('Reload the book before opening another contact', true);
      return;
    }
    selected = id;
    const contact = (book.contacts || []).find((entry) => entry.id === id);
    if (!contact) return;
    empty.hidden = true;
    form.hidden = false;
    document.getElementById('detail-name').textContent = contact.name || 'Unnamed';
    document.getElementById('detail-sub').textContent = contact.email || contact.role || '';
    for (const [key, input] of Object.entries(fields)) input.value = contact[key] || '';
    markDirty(false);
    setStatus('Open');
    window.__MYCELIUM_AGENT_CONTEXT = { app: 'People', detail: contact.name || contact.email || id };
    window.dispatchEvent(new CustomEvent('mycelium-agent-context', { detail: window.__MYCELIUM_AGENT_CONTEXT }));
    render();
  }

  function newContact() {
    if (conflict) {
      setStatus('Reload the book before adding another contact', true);
      return;
    }
    if (dirty) void saveBook();
    const id = `person-${Date.now().toString(36)}`;
    const contact = { id, name: '', email: '', phone: '', role: '', note: '', updated_at: new Date().toISOString() };
    book.contacts = [...(book.contacts || []), contact];
    selected = id;
    empty.hidden = true;
    form.hidden = false;
    document.getElementById('detail-name').textContent = 'New contact';
    document.getElementById('detail-sub').textContent = '';
    for (const input of Object.values(fields)) input.value = '';
    markDirty(true);
    setStatus('New contact — saving…');
    render();
    scheduleSave(40);
  }

  async function load() {
    try {
      const result = await api('/api/vfs/read?path=contacts.json');
      book = JSON.parse(result.text);
      version = result.version ?? null;
      setConflict(false);
      markDirty(false);
      setStatus('Book ready');
    } catch (error) {
      if (!/not found|no such/i.test(error.message)) {
        setStatus(error.message, true);
        return;
      }
      book = { contacts: [] };
      version = null;
      setConflict(false);
      markDirty(false);
      setStatus('New book');
    }
    if (!book.contacts?.some((contact) => contact.id === selected)) selected = null;
    if (!selected) {
      form.hidden = true;
      empty.hidden = false;
    }
    render();
  }

  async function saveBook() {
    if (conflict) return;
    if (saving) {
      queued = true;
      return;
    }
    clearTimeout(saveTimer);
    saving = true;
    const expected = version == null ? '' : `&expected_version=${version}`;
    const payload = JSON.stringify(book, null, 2);
    setStatus('Saving…');
    try {
      const result = await api(`/api/vfs/write?path=contacts.json${expected}`, {
        method: 'PUT',
        headers: { 'content-type': 'application/json; charset=utf-8' },
        body: payload,
      });
      version = result.version;
      if (JSON.stringify(book, null, 2) === payload) {
        markDirty(false);
        setStatus('Saved');
        render();
      } else {
        markDirty(true);
        queued = true;
      }
    } catch (error) {
      if (error.status === 409) {
        setConflict(true);
        markDirty(true);
        setStatus('Changed elsewhere — reload to continue', true);
      } else {
        setStatus(error.message, true);
      }
    } finally {
      saving = false;
      if (queued) {
        queued = false;
        if (!conflict) scheduleSave(50);
      }
    }
  }

  function readForm() {
    const contact = (book.contacts || []).find((entry) => entry.id === selected);
    if (!contact) return;
    for (const [key, input] of Object.entries(fields)) contact[key] = input.value.trim();
    contact.updated_at = new Date().toISOString();
    document.getElementById('detail-name').textContent = contact.name || 'Unnamed';
    document.getElementById('detail-sub').textContent = contact.email || contact.role || '';
  }

  async function deleteContact() {
    if (!selected || !confirm('Delete this contact?')) return;
    book.contacts = (book.contacts || []).filter((entry) => entry.id !== selected);
    selected = null;
    form.hidden = true;
    empty.hidden = false;
    markDirty(true);
    await saveBook();
    render();
  }

  for (const input of Object.values(fields)) {
    input.addEventListener('input', () => {
      readForm();
      markDirty(true);
      render();
      scheduleSave();
    });
  }
  document.getElementById('new-contact').addEventListener('click', newContact);
  document.getElementById('delete-contact').addEventListener('click', deleteContact);
  reload?.addEventListener('click', () => void load());
  document.addEventListener('keydown', (event) => {
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 's') {
      event.preventDefault();
      void saveBook();
    }
  });
  window.addEventListener('beforeunload', (event) => { if (dirty) event.preventDefault(); });
  load();
})();
