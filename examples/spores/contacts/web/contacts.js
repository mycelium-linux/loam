(() => {
  const list = document.getElementById('contact-list');
  const count = document.getElementById('count');
  const status = document.getElementById('status');
  const empty = document.getElementById('empty');
  const form = document.getElementById('form');
  const reload = document.getElementById('reload-contacts');
  const loadMore = document.getElementById('load-more');
  const searchInput = document.getElementById('contact-search');
  const tagEditor = document.getElementById('tag-editor');
  const tagList = document.getElementById('tag-list');
  const tagForm = document.getElementById('tag-form');
  const tagInput = document.getElementById('tag-input');
  const fields = {
    name: document.getElementById('field-name'),
    email: document.getElementById('field-email'),
    phone: document.getElementById('field-phone'),
    role: document.getElementById('field-role'),
    note: document.getElementById('field-note'),
  };
  const COLLECTION = 'contact';
  const SCHEMA = 'fungos.contacts/contact';
  const PAGE = 200;
  const contacts = new Map();
  let order = [];
  let selected = null;
  let cursor = null;
  let loading = false;
  let saving = false;
  let queued = false;
  let dirty = false;
  let conflict = false;
  let saveTimer = 0;
  let searchTimer = 0;
  let searchTerm = '';

  const decodeText = (base64) => {
    const binary = atob(base64);
    return new TextDecoder().decode(Uint8Array.from(binary, (char) => char.charCodeAt(0)));
  };
  const encodeText = (text) => {
    const bytes = new TextEncoder().encode(text);
    let binary = '';
    for (let index = 0; index < bytes.length; index += 0x8000) {
      binary += String.fromCharCode(...bytes.subarray(index, index + 0x8000));
    }
    return btoa(binary);
  };
  const setStatus = (text, error = false) => {
    status.textContent = text;
    status.className = error ? 'contact-error' : '';
  };
  const setConflict = (value) => {
    conflict = value;
    if (reload) reload.hidden = !value;
  };
  const firstValue = (values) => (Array.isArray(values) ? values.find((entry) => entry?.value)?.value || '' : '');
  const initials = (name) => (name || '?').trim().split(/\s+/).map((part) => part[0]).join('').slice(0, 2).toUpperCase();

  async function call(fn, params) {
    const id = crypto.randomUUID();
    const response = await fetch('/v1/call', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ v: 1, call: { id, interface: 'soil:records', function: fn, params } }),
    });
    const frame = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(frame.error || `${response.status} ${response.statusText}`);
    if (frame.result?.err) {
      const error = new Error(frame.result.err.message || frame.result.err.code);
      error.code = frame.result.err.code;
      throw error;
    }
    return frame.result?.ok;
  }

  function contactFromEntry(entry) {
    let body = {};
    try {
      body = JSON.parse(decodeText(entry.body || ''));
    } catch {}
    const name = body.display_name || [body.given_name, body.family_name].filter(Boolean).join(' ') || 'Unnamed';
    return {
      id: entry.key.id,
      schema: entry.schema || SCHEMA,
      revision: entry.revision || 0,
      origin: entry.origin || '',
      body,
      name,
      email: firstValue(body.emails),
      phone: firstValue(body.phones),
      role: body.title || body.organization || '',
      note: body.note || '',
      tags: Array.isArray(body.tags) ? body.tags : [],
      deleted: Boolean(entry.deleted),
    };
  }

  function bodyFromFields(contact) {
    const body = { ...(contact.body || {}) };
    const name = fields.name.value.trim();
    const parts = name.split(/\s+/).filter(Boolean);
    body.display_name = name;
    body.given_name = parts.shift() || '';
    body.family_name = parts.join(' ');
    body.emails = setFirst(body.emails, fields.email.value.trim(), 'INTERNET');
    body.phones = setFirst(body.phones, fields.phone.value.trim(), 'CELL');
    body.title = fields.role.value.trim();
    body.note = fields.note.value.trim();
    body.uid = body.uid || contact.id;
    body.last_modified = new Date().toISOString();
    for (const [key, value] of Object.entries({
      display_name: '', given_name: '', family_name: '', organization: '', title: '',
      emails: [], phones: [], addresses: [], urls: [], note: '', birthday: '',
      uid: '', created: '', last_modified: '', raw_vcard: '',
    })) {
      if (body[key] == null) body[key] = value;
    }
    return body;
  }

  function setFirst(values, value, label) {
    const next = Array.isArray(values) ? values.slice() : [];
    if (value) {
      if (next.length) next[0] = { ...next[0], value };
      else next.push({ label, value });
    } else if (next.length) {
      next.shift();
    }
    return next;
  }

  function renderTags(contact = contacts.get(selected)) {
    if (!tagList) return;
    tagList.replaceChildren();
    const list = contact?.tags || [];
    if (!list.length) {
      const quiet = document.createElement('span');
      quiet.className = 'tag-empty';
      quiet.textContent = 'No tags yet.';
      tagList.append(quiet);
      return;
    }
    for (const tag of list) {
      const chip = document.createElement('span');
      chip.className = 'tag-chip';
      chip.dataset.source = tag.source || 'manual';
      chip.title = tag.source === 'inferred'
        ? `Suggested${tag.confidence != null ? ` · confidence ${Math.round(tag.confidence * 100)}%` : ''}`
        : 'Manual tag';
      chip.append(document.createTextNode(tag.name || ''));
      const remove = document.createElement('button');
      remove.type = 'button';
      remove.setAttribute('aria-label', `Remove ${tag.name || 'tag'}`);
      remove.textContent = '×';
      remove.addEventListener('click', () => removeTag(tag.name));
      chip.append(remove);
      tagList.append(chip);
    }
  }

  function addTag(event) {
    event.preventDefault();
    const contact = selected ? contacts.get(selected) : null;
    const name = tagInput.value.trim();
    if (!contact || !name) return;
    if (contact.tags.some((tag) => tag.name.toLowerCase() === name.toLowerCase())) {
      tagInput.value = '';
      return;
    }
    contact.tags = [...contact.tags, { name, source: 'manual', status: 'active', confidence: 1 }];
    contact.body = { ...(contact.body || {}), tags: contact.tags };
    tagInput.value = '';
    dirty = true;
    renderTags(contact);
    setStatus('Tag added');
    scheduleSave(40);
  }

  function removeTag(name) {
    const contact = selected ? contacts.get(selected) : null;
    if (!contact) return;
    contact.tags = contact.tags.filter((tag) => tag.name !== name);
    contact.body = { ...(contact.body || {}), tags: contact.tags };
    dirty = true;
    renderTags(contact);
    setStatus('Tag removed');
    scheduleSave(40);
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
    count.textContent = searchTerm
      ? `${order.length} match${order.length === 1 ? '' : 'es'}${cursor ? '+' : ''}`
      : `${order.length} loaded${cursor ? '' : ' · all'}`;
    if (!order.length) {
      const quiet = document.createElement('p');
      quiet.className = 'contact-empty';
      quiet.textContent = searchTerm ? 'No matches.' : 'No contacts yet.';
      list.append(quiet);
    } else {
      for (const id of order) {
        const contact = contacts.get(id);
        if (contact) list.append(row(contact));
      }
    }
    if (loadMore) loadMore.hidden = !cursor;
  }

  function sortOrder() {
    order.sort((left, right) => (contacts.get(left)?.name || '').localeCompare(contacts.get(right)?.name || '', undefined, { sensitivity: 'base' }));
  }

  async function loadPage({ reset = false } = {}) {
    if (loading) return;
    if (reset) {
      contacts.clear();
      order = [];
      cursor = null;
      selected = null;
      form.hidden = true;
      empty.hidden = false;
      if (tagEditor) tagEditor.hidden = true;
      renderTags();
    }
    loading = true;
    if (loadMore) loadMore.disabled = true;
    setStatus(searchTerm ? `Searching for “${searchTerm}”…` : 'Loading records…');
    try {
      const page = searchTerm
        ? await call('query', {
            collection: COLLECTION,
            filters: [{ field: 'display_name', op: 'contains', value: searchTerm }],
            cursor,
            limit: PAGE,
          })
        : await call('scan', { collection: COLLECTION, cursor, limit: PAGE });
      for (const entry of page.entries || []) {
        const contact = contactFromEntry(entry);
        if (!contacts.has(contact.id)) order.push(contact.id);
        contacts.set(contact.id, contact);
      }
      cursor = page.next || null;
      sortOrder();
      render();
      setStatus(searchTerm
        ? `Found ${order.length}${cursor ? '+' : ''} for “${searchTerm}”`
        : `Loaded ${order.length}${cursor ? '+' : ''} contacts`);
    } catch (error) {
      setStatus(error.message, true);
    } finally {
      loading = false;
      if (loadMore) loadMore.disabled = false;
    }
  }

  async function select(id) {
    if (dirty && !conflict) await saveContact();
    if (conflict) {
      setStatus('Reload this contact before opening another', true);
      return;
    }
    const contact = contacts.get(id);
    if (!contact) return;
    selected = id;
    empty.hidden = true;
    form.hidden = false;
    document.getElementById('detail-name').textContent = contact.name || 'Unnamed';
    document.getElementById('detail-sub').textContent = contact.email || contact.role || '';
    fields.name.value = contact.body?.display_name || contact.name || '';
    fields.email.value = contact.email || '';
    fields.phone.value = contact.phone || '';
    fields.role.value = contact.role || '';
    fields.note.value = contact.note || '';
    dirty = false;
    setConflict(false);
    if (tagEditor) tagEditor.hidden = false;
    renderTags(contact);
    setStatus('Open');
    window.__MYCELIUM_AGENT_CONTEXT = { app: 'People', detail: contact.name || contact.email || id };
    window.dispatchEvent(new CustomEvent('mycelium-agent-context', { detail: window.__MYCELIUM_AGENT_CONTEXT }));
    render();
  }

  function newContact() {
    if (conflict) {
      setStatus('Reload the current contact before adding another', true);
      return;
    }
    if (dirty) void saveContact();
    const id = `contact-${crypto.randomUUID().replaceAll('-', '')}`;
    const body = {
      display_name: '', given_name: '', family_name: '', organization: '', title: '',
      emails: [], phones: [], addresses: [], urls: [], note: '', birthday: '',
      uid: id, created: new Date().toISOString(), last_modified: '', raw_vcard: '',
    };
    const contact = contactFromEntry({
      key: { collection: COLLECTION, id },
      schema: SCHEMA,
      revision: 0,
      origin: '',
      body: encodeText(JSON.stringify(body)),
      deleted: false,
    });
    contacts.set(id, contact);
    order.push(id);
    selected = id;
    empty.hidden = true;
    form.hidden = false;
    document.getElementById('detail-name').textContent = 'New contact';
    document.getElementById('detail-sub').textContent = '';
    for (const input of Object.values(fields)) input.value = '';
    if (tagEditor) tagEditor.hidden = false;
    renderTags(contact);
    dirty = true;
    setStatus('New contact — saving…');
    sortOrder();
    render();
    scheduleSave(40);
  }

  function scheduleSave(delay = 700) {
    if (!selected || conflict) return;
    clearTimeout(saveTimer);
    saveTimer = window.setTimeout(() => void saveContact(), delay);
  }

  async function saveContact() {
    if (!selected || conflict) return;
    if (saving) {
      queued = true;
      return;
    }
    const contact = contacts.get(selected);
    if (!contact || contact.deleted) return;
    clearTimeout(saveTimer);
    saving = true;
    const snapshot = Object.fromEntries(Object.entries(fields).map(([key, input]) => [key, input.value]));
    const body = bodyFromFields(contact);
    const expected = contact.revision ? { revision: contact.revision, origin: contact.origin } : null;
    setStatus('Saving…');
    try {
      const result = await call('compare-and-swap', {
        key: { collection: COLLECTION, id: contact.id },
        expected,
        entry: {
          key: { collection: COLLECTION, id: contact.id },
          schema: contact.schema || SCHEMA,
          deleted: false,
          body: encodeText(JSON.stringify(body)),
        },
      });
      if (!result.applied) {
        const current = contactFromEntry(result.entry);
        contacts.set(current.id, current);
        setConflict(true);
        dirty = true;
        setStatus('Changed elsewhere — reload to continue', true);
        return;
      }
      const saved = contactFromEntry(result.entry);
      contacts.set(saved.id, saved);
      const unchanged = Object.entries(fields).every(([key, input]) => snapshot[key] === input.value);
      if (selected === saved.id) {
        document.getElementById('detail-name').textContent = saved.name || 'Unnamed';
        document.getElementById('detail-sub').textContent = saved.email || saved.role || '';
      }
      dirty = !unchanged;
      setStatus(unchanged ? 'Saved' : 'Editing…');
      sortOrder();
      render();
      if (!unchanged) queued = true;
    } catch (error) {
      setStatus(error.message, true);
    } finally {
      saving = false;
      if (queued) {
        queued = false;
        if (!conflict && dirty) scheduleSave(50);
      }
    }
  }

  async function reloadContact() {
    if (!selected) return;
    try {
      const entry = await call('get', { key: { collection: COLLECTION, id: selected } });
      const contact = contactFromEntry(entry);
      contacts.set(contact.id, contact);
      setConflict(false);
      dirty = false;
      await select(contact.id);
      setStatus('Reloaded');
    } catch (error) {
      setStatus(error.message, true);
    }
  }

  async function deleteContact() {
    const contact = selected ? contacts.get(selected) : null;
    if (!contact || !confirm('Delete this contact?')) return;
    try {
      const result = await call('compare-and-swap', {
        key: { collection: COLLECTION, id: contact.id },
        expected: contact.revision ? { revision: contact.revision, origin: contact.origin } : null,
        entry: {
          key: { collection: COLLECTION, id: contact.id },
          schema: contact.schema || SCHEMA,
          deleted: true,
          body: '',
        },
      });
      if (!result.applied) {
        setConflict(true);
        setStatus('Changed elsewhere — reload before deleting', true);
        return;
      }
      contacts.delete(contact.id);
      order = order.filter((id) => id !== contact.id);
      selected = null;
      form.hidden = true;
      empty.hidden = false;
      if (tagEditor) tagEditor.hidden = true;
      renderTags();
      dirty = false;
      setConflict(false);
      setStatus('Deleted');
      render();
    } catch (error) {
      setStatus(error.message, true);
    }
  }

  for (const input of Object.values(fields)) {
    input.addEventListener('input', () => {
      dirty = true;
      setStatus('Editing…');
      scheduleSave();
    });
  }
  searchInput?.addEventListener('input', () => {
    clearTimeout(searchTimer);
    searchTimer = window.setTimeout(async () => {
      searchTerm = searchInput.value.trim();
      if (dirty && !conflict) await saveContact();
      if (conflict) {
        setStatus('Reload the current contact before searching', true);
        return;
      }
      await loadPage({ reset: true });
    }, 300);
  });
  tagForm?.addEventListener('submit', addTag);
  document.getElementById('new-contact').addEventListener('click', newContact);
  document.getElementById('delete-contact').addEventListener('click', deleteContact);
  reload?.addEventListener('click', () => void reloadContact());
  loadMore?.addEventListener('click', () => void loadPage());
  document.addEventListener('keydown', (event) => {
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 's') {
      event.preventDefault();
      void saveContact();
    }
  });
  window.addEventListener('beforeunload', (event) => { if (dirty) event.preventDefault(); });
  void loadPage();
})();
