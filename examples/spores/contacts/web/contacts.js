(() => {
  const list = document.getElementById('contact-list');
  const count = document.getElementById('count');
  const status = document.getElementById('status');
  const empty = document.getElementById('empty');
  const form = document.getElementById('form');
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

  const desktop = new URLSearchParams(location.search).get('desktop') || 'personal';
  const withDesktop = (path) => `${path}${path.includes('?') ? '&' : '?'}desktop=${encodeURIComponent(desktop)}`;
  const api = async (path, options = {}) => {
    const response = await fetch(withDesktop(path), options);
    const value = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(value.error || `${response.status} ${response.statusText}`);
    return value;
  };
  const setStatus = (text, error = false) => {
    status.textContent = text;
    status.className = error ? 'contact-error' : '';
  };
  const markDirty = (value) => {
    dirty = value;
    if (value) setStatus('Unsaved changes');
  };
  const initials = (name) => (name || '?').trim().split(/\s+/).map((part) => part[0]).join('').slice(0, 2).toUpperCase();

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
    button.addEventListener('click', () => select(contact.id));
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

  function select(id) {
    if (dirty && !confirm('Discard unsaved changes?')) return;
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
    if (dirty && !confirm('Discard unsaved changes?')) return;
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
    setStatus('New contact — save when ready');
    render();
  }

  async function load() {
    try {
      const result = await api('/api/vfs/read?path=contacts.json');
      book = JSON.parse(result.text);
      version = result.version ?? null;
      setStatus('Book ready');
    } catch (error) {
      if (!/not found|no such/i.test(error.message)) {
        setStatus(error.message, true);
        return;
      }
      book = { contacts: [] };
      version = null;
      setStatus('New book');
    }
    render();
  }

  async function saveBook() {
    try {
      const path = 'contacts.json';
      const expected = version == null ? '' : `&expected_version=${version}`;
      const result = await api(`/api/vfs/write?path=${path}${expected}`, {
        method: 'PUT',
        headers: { 'content-type': 'application/json; charset=utf-8' },
        body: JSON.stringify(book, null, 2),
      });
      version = result.version;
      markDirty(false);
      setStatus('Saved');
      render();
    } catch (error) {
      setStatus(error.message, true);
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

  async function saveContact() {
    readForm();
    await saveBook();
    render();
  }

  async function deleteContact() {
    if (!selected || !confirm('Delete this contact?')) return;
    book.contacts = (book.contacts || []).filter((entry) => entry.id !== selected);
    selected = null;
    form.hidden = true;
    empty.hidden = false;
    await saveBook();
  }

  for (const input of Object.values(fields)) input.addEventListener('input', () => markDirty(true));
  document.getElementById('new-contact').addEventListener('click', newContact);
  document.getElementById('save-contact').addEventListener('click', saveContact);
  document.getElementById('delete-contact').addEventListener('click', deleteContact);
  document.addEventListener('keydown', (event) => {
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 's') {
      event.preventDefault();
      saveContact();
    }
  });
  window.addEventListener('beforeunload', (event) => { if (dirty) event.preventDefault(); });
  load();
})();
