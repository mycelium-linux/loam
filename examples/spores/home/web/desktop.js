(() => {
  const root = document.documentElement;
  const windows = new Map();
  let z = 10;
  let desktops = [];
  let activeDesktop = localStorage.getItem('mycelium.desktop') || 'personal';
  let activeDesktopName = 'This Mac';

  for (const el of document.querySelectorAll('[data-window]')) {
    windows.set(el.dataset.window, el);
    const saved = localStorage.getItem(`mycelium.bounds.${el.dataset.window}`);
    if (saved) {
      try {
        const bounds = JSON.parse(saved);
        for (const key of ['left', 'top', 'width', 'height']) {
          if (Number.isFinite(bounds[key])) el.style[key] = `${bounds[key]}px`;
        }
      } catch {}
    }
  }

  const appLabels = { notes: 'Notes', contacts: 'People', files: 'Files', desk: 'This Desk' };
  function setAgentContext(kind = 'desk') {
    const detail = appLabels[kind] || 'Desktop';
    window.__MYCELIUM_AGENT_CONTEXT = { app: 'This Mac', detail: kind === 'desk' ? 'Desktop' : detail };
    window.dispatchEvent(new CustomEvent('mycelium-agent-context', {
      detail: window.__MYCELIUM_AGENT_CONTEXT,
    }));
  }

  function focusWindow(el) {
    el.style.zIndex = String(++z);
    if (el.dataset.window) setAgentContext(el.dataset.window);
  }

  function frameUrl(frame) {
    const url = new URL(frame.dataset.src);
    if (activeDesktop) url.searchParams.set('desktop', activeDesktop);
    return url.toString();
  }

  function open(kind) {
    const el = windows.get(kind);
    if (!el) return;
    el.hidden = false;
    el.classList.add('is-open');
    const frame = el.querySelector('iframe[data-src]');
    if (frame) {
      const next = frameUrl(frame);
      if (frame.getAttribute('src') !== next) frame.src = next;
    }
    focusWindow(el);
  }

  function close(kind) {
    const el = windows.get(kind);
    if (!el) return;
    el.hidden = true;
    el.classList.remove('is-open');
    const frame = el.querySelector('iframe');
    if (frame) frame.src = 'about:blank';
    setAgentContext('desk');
  }

  document.addEventListener('click', (event) => {
    const openButton = event.target.closest('[data-open]');
    if (openButton) {
      event.preventDefault();
      const kind = openButton.dataset.open;
      open(kind);
      closeAccountMenu();
      closeCommand();
      return;
    }
    const wizardSkip = event.target.closest('[data-wizard-skip]');
    if (wizardSkip) {
      event.preventDefault();
      closeOrientation();
      return;
    }
    const wizardNext = event.target.closest('[data-wizard-next]');
    if (wizardNext) {
      event.preventDefault();
      if (!wizardNext.disabled) setWizardStep(Number(onboarding?.dataset.step || 1) + 1);
      return;
    }
    const wizardBack = event.target.closest('[data-wizard-back]');
    if (wizardBack) {
      event.preventDefault();
      setWizardStep(Number(onboarding?.dataset.step || 1) - 1);
      return;
    }
    const wizardWhere = event.target.closest('[data-where]');
    if (wizardWhere) {
      event.preventDefault();
      const previous = wizardAnswers.where;
      wizardAnswers.where = wizardWhere.dataset.where;
      if (previous !== wizardAnswers.where) {
        if (wizardAnswers.where === 'myself' && (!wizardAnswers.desktopName || wizardAnswers.desktopName === 'Work')) {
          wizardAnswers.desktopName = 'Personal';
        }
        if (wizardAnswers.where === 'business' && (!wizardAnswers.desktopName || wizardAnswers.desktopName === 'Personal')) {
          wizardAnswers.desktopName = wizardAnswers.businessName || 'Work';
        }
      }
      syncNameFields();
      paintWizardChoices();
      refreshWizardControls();
      return;
    }
    const wizardWhat = event.target.closest('[data-what]');
    if (wizardWhat) {
      event.preventDefault();
      wizardAnswers.what = wizardWhat.dataset.what;
      paintWizardChoices();
      refreshWizardControls();
      return;
    }
    const wizardStart = event.target.closest('[data-wizard-start]');
    if (wizardStart) {
      event.preventDefault();
      void (async () => {
        const desktop = await ensureDesktop();
        saveWizardAnswers();
        localStorage.setItem('mycelium.desktop', desktop.id);
        await loadDesktops();
        applyDesktop(desktop.id, desktop.name);
        closeOrientation();
        open(wizardAnswers.what === 'website' ? 'files' : 'notes');
        window.setTimeout(() => window.MyceliumAgent?.seed(buildAgentSeed()), 420);
      })();
      return;
    }
    const wizardOpen = event.target.closest('[data-wizard-open]');
    if (wizardOpen) {
      event.preventDefault();
      closeOrientation();
      open(wizardOpen.dataset.wizardOpen);
      return;
    }
    const orientationTrigger = event.target.closest('[data-orientation]');
    if (orientationTrigger) {
      event.preventDefault();
      closeAccountMenu();
      openOrientation();
      return;
    }
    const closeButton = event.target.closest('[data-close]');
    if (closeButton) {
      event.preventDefault();
      close(closeButton.dataset.close);
      return;
    }
    const minimizeButton = event.target.closest('[data-minimize]');
    if (minimizeButton) {
      event.preventDefault();
      const el = windows.get(minimizeButton.dataset.minimize);
      if (el) el.hidden = true;
      return;
    }
    const zoomButton = event.target.closest('[data-zoom]');
    if (zoomButton) {
      event.preventDefault();
      const el = windows.get(zoomButton.dataset.zoom);
      if (!el) return;
      const zoomed = el.dataset.zoomed === 'true';
      if (zoomed) {
        el.style.inset = '';
        el.style.width = '';
        el.style.height = '';
        el.dataset.zoomed = 'false';
      } else {
        el.style.inset = '42px 8px 78px';
        el.style.width = 'auto';
        el.style.height = 'auto';
        el.dataset.zoomed = 'true';
      }
      focusWindow(el);
    }
  });

  document.addEventListener('pointerdown', (event) => {
    const el = event.target.closest('[data-window]');
    if (el) focusWindow(el);
  });

  function saveBounds(el) {
    const rect = el.getBoundingClientRect();
    localStorage.setItem(`mycelium.bounds.${el.dataset.window}`, JSON.stringify({
      left: Math.round(rect.left),
      top: Math.round(rect.top),
      width: Math.round(rect.width),
      height: Math.round(rect.height),
    }));
  }

  let drag = null;
  let resize = null;

  document.addEventListener('pointerdown', (event) => {
    const handle = event.target.closest('[data-resize]');
    if (handle) {
      const el = windows.get(handle.dataset.resize);
      if (!el) return;
      const rect = el.getBoundingClientRect();
      resize = {
        el,
        startX: event.clientX,
        startY: event.clientY,
        width: rect.width,
        height: rect.height,
      };
      handle.setPointerCapture?.(event.pointerId);
      event.preventDefault();
      return;
    }
    const bar = event.target.closest('.window-bar');
    if (!bar || event.target.closest('button')) return;
    const el = bar.closest('[data-window]');
    if (!el) return;
    const rect = el.getBoundingClientRect();
    drag = {
      el,
      startX: event.clientX,
      startY: event.clientY,
      left: rect.left,
      top: rect.top,
    };
    bar.setPointerCapture?.(event.pointerId);
    event.preventDefault();
  });

  document.addEventListener('pointermove', (event) => {
    if (drag) {
      drag.el.style.inset = 'auto';
      drag.el.style.left = `${drag.left + event.clientX - drag.startX}px`;
      drag.el.style.top = `${drag.top + event.clientY - drag.startY}px`;
      drag.el.dataset.zoomed = 'false';
      return;
    }
    if (resize) {
      resize.el.style.inset = 'auto';
      resize.el.style.width = `${Math.max(320, resize.width + event.clientX - resize.startX)}px`;
      resize.el.style.height = `${Math.max(220, resize.height + event.clientY - resize.startY)}px`;
      resize.el.dataset.zoomed = 'false';
    }
  });

  document.addEventListener('pointerup', () => {
    if (drag) saveBounds(drag.el);
    if (resize) saveBounds(resize.el);
    drag = null;
    resize = null;
  });

  const command = document.getElementById('command');
  const scrim = document.querySelector('[data-command-scrim]');
  const input = document.getElementById('command-input');

  function openCommand() {
    if (!command) return;
    command.hidden = false;
    command.removeAttribute('inert');
    command.setAttribute('aria-hidden', 'false');
    root.dataset.command = 'open';
    if (scrim) scrim.hidden = false;
    input?.focus();
  }
  function closeCommand() {
    if (!command || command.hidden) return;
    command.hidden = true;
    command.setAttribute('inert', '');
    command.setAttribute('aria-hidden', 'true');
    root.dataset.command = 'closed';
    if (scrim) scrim.hidden = true;
  }

  for (const button of document.querySelectorAll('[data-command-toggle]')) {
    button.addEventListener('click', openCommand);
  }
  scrim?.addEventListener('click', closeCommand);
  document.addEventListener('keydown', (event) => {
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k') {
      event.preventDefault();
      openCommand();
    }
    if (event.key === 'Escape') closeCommand();
  });

  document.getElementById('command-form')?.addEventListener('submit', (event) => {
    event.preventDefault();
    const value = input.value.trim().toLowerCase();
    const match = value.match(/notes|people|contacts|files/);
    if (match) {
      open(match[0] === 'people' ? 'contacts' : match[0]);
      closeCommand();
    }
  });


  const desktopName = document.getElementById('desktop-name');
  const desktopSwitcher = document.getElementById('desktop-switcher');
  const desktopGrid = document.getElementById('desktop-grid');
  const accountName = document.getElementById('account-name');

  function applyDesktop(id, name, reload = true) {
    activeDesktop = id || 'personal';
    activeDesktopName = name || 'This Mac';
    try { localStorage.setItem('mycelium.desktop', activeDesktop); } catch {}
    if (desktopName) desktopName.textContent = activeDesktopName;
    if (accountName) accountName.textContent = activeDesktopName;
    if (accountWorkspace) accountWorkspace.textContent = `~/fungOS/${activeDesktop}`;
    if (reload) {
      for (const el of windows.values()) {
        const frame = el.querySelector('iframe[data-src]');
        if (frame && !el.hidden) frame.src = frameUrl(frame);
      }
    }
    setAgentContext('desk');
  }

  function renderDesktopGrid() {
    if (!desktopGrid) return;
    desktopGrid.replaceChildren();
    for (const desktop of desktops) {
      const button = document.createElement('button');
      button.className = 'profile-button';
      button.type = 'button';
      button.dataset.desktopSelect = desktop.id;
      button.setAttribute('aria-current', String(desktop.id === activeDesktop));
      const circle = document.createElement('span');
      circle.className = `profile-circle profile-circle--${desktop.kind === 'personal' ? 'personal' : 'work'}`;
      circle.textContent = (desktop.name || '?').slice(0, 1).toUpperCase();
      const name = document.createElement('span');
      name.className = 'profile-name';
      name.textContent = desktop.name;
      const kind = document.createElement('span');
      kind.className = 'profile-kind';
      kind.textContent = desktop.kind === 'personal' ? 'Personal' : 'Work';
      const path = document.createElement('span');
      path.className = 'profile-meta';
      path.textContent = `~/fungOS/${desktop.id}`;
      button.append(circle, name, kind, path);
      if (desktop.id === activeDesktop) {
        const current = document.createElement('span');
        current.className = 'profile-current';
        current.textContent = 'Current';
        button.append(current);
      }
      button.addEventListener('click', () => {
        applyDesktop(desktop.id, desktop.name);
        desktopSwitcher.hidden = true;
      });
      desktopGrid.append(button);
    }
  }

  async function loadDesktops() {
    try {
      const response = await fetch('/api/vfs/desktops');
      const data = await response.json();
      desktops = Array.isArray(data.desktops) ? data.desktops : [];
    } catch {
      desktops = [];
    }
    if (!desktops.length) desktops = [{ id: 'personal', name: 'Personal', kind: 'personal' }];
    const selected = desktops.find((desktop) => desktop.id === activeDesktop) || desktops[0];
    applyDesktop(selected.id, selected.name, false);
    renderDesktopGrid();
  }

  document.querySelector('[data-desktop-switcher]')?.addEventListener('click', () => {
    renderDesktopGrid();
    desktopSwitcher.hidden = false;
  });
  document.querySelectorAll('[data-desktop-close]').forEach((button) => {
    button.addEventListener('click', () => { desktopSwitcher.hidden = true; });
  });
  desktopSwitcher?.addEventListener('click', (event) => {
    if (event.target === desktopSwitcher) desktopSwitcher.hidden = true;
  });

  const accountToggle = document.getElementById('account-toggle');
  const accountMenu = document.getElementById('account-menu');
  const accountWorkspace = document.getElementById('account-workspace');

  function openAccountMenu() {
    if (!accountMenu) return;
    accountMenu.hidden = false;
    accountToggle?.setAttribute('aria-expanded', 'true');
  }
  function closeAccountMenu() {
    if (!accountMenu || accountMenu.hidden) return;
    accountMenu.hidden = true;
    accountToggle?.setAttribute('aria-expanded', 'false');
  }
  accountToggle?.addEventListener('click', (event) => {
    event.stopPropagation();
    accountMenu?.hidden ? openAccountMenu() : closeAccountMenu();
  });
  document.addEventListener('pointerdown', (event) => {
    if (!accountMenu || accountMenu.hidden) return;
    if (accountMenu.contains(event.target) || accountToggle?.contains(event.target)) return;
    closeAccountMenu();
  }, true);


  const onboarding = document.getElementById('onboarding');
  const connectionsTitle = document.getElementById('connections-title');
  const connectionsCopy = document.getElementById('connections-copy');
  const connectionsList = document.getElementById('connections-list');
  const businessField = document.getElementById('business-field');
  const businessNameInput = document.getElementById('business-name');
  const desktopNameInput = document.getElementById('desktop-name');
  let wizardStep = 1;
  let wizardAnswers = { where: null, what: null, desktopName: '', businessName: '' };

  const fplAuthBase = 'https://sso.fpl.dev';
  const providerCatalog = {
    cloudflare: { label: 'Cloudflare', slug: 'cloudflare', note: 'Domains, DNS, and the public edge', href: `${fplAuthBase}/connections/cloudflare/start?owner=user` },
    github: { label: 'GitHub', slug: 'github', note: 'Repositories, issues, and workflows', href: `${fplAuthBase}/connections/github/start?owner=user` },
    google: { label: 'Google', slug: 'google', note: 'Gmail, Calendar, and Drive', href: `${fplAuthBase}/connections/google/start?owner=user` },
    openai: { label: 'OpenAI', slug: 'openai', note: 'ChatGPT / Codex subscription', href: `${fplAuthBase}/connections`, logo: 'https://cdn.jsdelivr.net/npm/@lobehub/icons-static-svg@latest/icons/openai.svg', light: true },
    xai: { label: 'xAI', slug: 'x', note: 'Grok / X Premium subscription', href: `${fplAuthBase}/connections` },
    openrouter: { label: 'OpenRouter', slug: 'openrouter', note: 'Model routing and API keys', href: `${fplAuthBase}/connections` },
    hue: { label: 'Philips Hue', slug: 'philipshue', note: 'Smart lights and local scenes', href: `${fplAuthBase}/connections/philips-hue/start?owner=user` },
    spotify: { label: 'Spotify', slug: 'spotify', note: 'Playback, search, and playlists', href: `${fplAuthBase}/connections/spotify/start?owner=user` },
  };

  const recommendationIds = {
    website: { myself: ['cloudflare', 'github', 'google', 'openai'], business: ['cloudflare', 'github', 'google', 'openai'] },
    automation: { myself: ['github', 'google', 'hue', 'spotify'], business: ['github', 'google', 'cloudflare', 'openai'] },
    agent: { myself: ['openai', 'xai', 'github', 'openrouter'], business: ['openai', 'xai', 'github', 'openrouter'] },
    presentation: { myself: ['google', 'openai', 'github', 'cloudflare'], business: ['google', 'openai', 'github', 'cloudflare'] },
    document: { myself: ['google', 'openai', 'github', 'cloudflare'], business: ['google', 'openai', 'github', 'cloudflare'] },
  };

  const whatLabels = {
    website: 'a website',
    automation: 'an automation',
    agent: 'an agent',
    presentation: 'a presentation',
    document: 'a document',
  };
  const whatTitles = {
    website: 'Website',
    automation: 'Automation',
    agent: 'Agent',
    presentation: 'Presentation',
    document: 'Document',
  };

  function syncNameFields() {
    const business = wizardAnswers.where === 'business';
    if (businessField) businessField.hidden = !business;
    if (wizardAnswers.where === 'myself' && (!wizardAnswers.desktopName || wizardAnswers.desktopName === 'Work')) {
      wizardAnswers.desktopName = 'Personal';
    }
    if (business && (!wizardAnswers.desktopName || wizardAnswers.desktopName === 'Personal')) {
      wizardAnswers.desktopName = wizardAnswers.businessName || 'Work';
    }
    if (businessNameInput) businessNameInput.value = wizardAnswers.businessName || '';
    if (desktopNameInput) desktopNameInput.value = wizardAnswers.desktopName || '';
  }

  function slug(value) {
    return value.trim().toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '') || 'work';
  }

  function buildDesktop() {
    const name = (wizardAnswers.desktopName || '').trim();
    const business = (wizardAnswers.businessName || '').trim();
    let prefix = slug(name || business || (wizardAnswers.where === 'business' ? 'work' : 'personal'));
    if (wizardAnswers.where === 'business' && !business && (!name || name.toLowerCase() === 'work')) prefix = 'work';
    if (wizardAnswers.where !== 'business' && prefix !== 'personal' && !name) prefix = 'personal';
    return {
      id: prefix,
      name: name || business || (wizardAnswers.where === 'business' ? 'Work' : 'Personal'),
      kind: wizardAnswers.where === 'business' ? 'work' : 'personal',
      business,
    };
  }

  async function ensureDesktop() {
    const desktop = buildDesktop();
    try {
      await fetch(`/api/vfs/write?path=desktop.json&desktop=${encodeURIComponent(desktop.id)}`, {
        method: 'PUT',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ name: desktop.name, kind: desktop.kind, business: desktop.business }),
      });
    } catch {}
    return desktop;
  }

  function paintWizardChoices() {
    onboarding?.querySelectorAll('[data-where]').forEach((node) => {
      node.classList.toggle('is-selected', node.dataset.where === wizardAnswers.where);
      node.setAttribute('aria-pressed', String(node.dataset.where === wizardAnswers.where));
    });
    onboarding?.querySelectorAll('[data-what]').forEach((node) => {
      node.classList.toggle('is-selected', node.dataset.what === wizardAnswers.what);
      node.setAttribute('aria-pressed', String(node.dataset.what === wizardAnswers.what));
    });
  }

  function refreshWizardControls() {
    const whereNext = onboarding?.querySelector('[data-wizard-step="4"] [data-wizard-next]');
    const whatNext = onboarding?.querySelector('[data-wizard-step="5"] [data-wizard-next]');
    if (whereNext) whereNext.disabled = !wizardAnswers.where || !wizardAnswers.desktopName.trim();
    if (whatNext) whatNext.disabled = !wizardAnswers.what;
  }

  function accountLogo(provider) {
    const mark = document.createElement('span');
    mark.className = 'account-logo';
    if (!provider.slug) {
      mark.textContent = provider.label.slice(0, 1);
      return mark;
    }
    const image = document.createElement('img');
    image.alt = '';
    image.loading = 'lazy';
    image.src = provider.logo || `https://cdn.simpleicons.org/${provider.slug}/e5e7eb`;
    if (provider.light) image.className = 'is-light';
    image.addEventListener('error', () => {
      image.remove();
      mark.textContent = provider.label.slice(0, 1);
    });
    mark.append(image);
    return mark;
  }

  function renderConnections() {
    if (!connectionsList) return;
    const where = wizardAnswers.where || 'myself';
    const what = wizardAnswers.what || 'document';
    const ids = recommendationIds[what]?.[where] || recommendationIds.document.myself;
    connectionsList.replaceChildren();
    if (connectionsTitle) connectionsTitle.textContent = 'Connect what you already use.';
    if (connectionsCopy) {
      connectionsCopy.textContent = `For ${where === 'business' ? 'a business' : 'yourself'}, starting with ${whatLabels[what]}, these are the first accounts we would prioritize.`;
    }
    for (const id of ids) {
      const provider = providerCatalog[id];
      if (!provider) continue;
      const card = document.createElement('div');
      card.className = 'account-card';
      const copy = document.createElement('span');
      copy.className = 'account-copy';
      const strong = document.createElement('strong');
      strong.textContent = provider.label;
      const small = document.createElement('small');
      small.textContent = provider.note;
      copy.append(strong, small);
      const link = document.createElement('a');
      link.className = 'account-connect';
      link.href = provider.href;
      link.target = '_blank';
      link.rel = 'noopener';
      link.textContent = 'Connect';
      card.append(accountLogo(provider), copy, link);
      connectionsList.append(card);
    }
  }

  function buildAgentSeed() {
    const where = wizardAnswers.where || 'myself';
    const what = wizardAnswers.what || 'document';
    const owner = where === 'business' ? 'my business' : 'myself';
    const prompts = {
      website: `Set up my first website for ${owner}. Start with the homepage and the accounts I need.`,
      automation: 'Build my first automation. Connect the tools that already hold the work.',
      agent: 'Build an agent that can work inside my apps. Start with one useful job.',
      presentation: 'Help me make my first presentation from what is already on this desk.',
      document: 'Start my first document and shape it from the workspace.',
    };
    const ready = {
      website: 'I will start with the site files in ~/fungOS. Connect Cloudflare and GitHub through FPL Auth, and Geode will hold the credentials.',
      automation: 'I will start with the workflow in the workspace. Connect Google and GitHub through FPL Auth so the steps have something real to touch.',
      agent: 'I will start with Notes and the agent surface. Connect OpenAI or xAI through FPL Auth; Geode owns the provider key.',
      presentation: 'I will start with Notes and turn the workspace into an outline. Connect Google through FPL Auth when you want slides.',
      document: 'I will start with Notes and the VFS. Connect Google through FPL Auth when the document needs to leave this Mac.',
    };
    const tools = {
      website: [
        { label: 'VFS', note: `~/fungOS/${buildDesktop().id} site files`, state: 'preparing' },
        { label: 'Frontdoor', note: 'local preview', state: 'preparing' },
        { label: 'FPL Auth', note: 'Cloudflare · GitHub', state: 'waiting' },
        { label: 'Geode', note: 'credential vault', state: 'preparing' },
      ],
      automation: [
        { label: 'VFS', note: `~/fungOS/${buildDesktop().id} workspace`, state: 'preparing' },
        { label: 'Loam', note: 'run the first step', state: 'preparing' },
        { label: 'FPL Auth', note: 'Google · GitHub', state: 'waiting' },
        { label: 'Geode', note: 'credential vault', state: 'preparing' },
      ],
      agent: [
        { label: 'Loam', note: 'agent harness', state: 'preparing' },
        { label: 'VFS', note: `~/fungOS/${buildDesktop().id} notes`, state: 'preparing' },
        { label: 'FPL Auth', note: 'OpenAI · xAI', state: 'waiting' },
        { label: 'Geode', note: 'provider keys', state: 'preparing' },
      ],
      presentation: [
        { label: 'Notes', note: 'outline and slides', state: 'preparing' },
        { label: 'VFS', note: `~/fungOS/${buildDesktop().id} source`, state: 'preparing' },
        { label: 'FPL Auth', note: 'Google', state: 'waiting' },
        { label: 'Geode', note: 'credential vault', state: 'preparing' },
      ],
      document: [
        { label: 'Notes', note: 'long-form editor', state: 'preparing' },
        { label: 'VFS', note: `~/fungOS/${buildDesktop().id} workspace`, state: 'preparing' },
        { label: 'FPL Auth', note: 'Google', state: 'waiting' },
        { label: 'Geode', note: 'credential vault', state: 'preparing' },
      ],
    };
    return {
      prompt: prompts[what] || prompts.document,
      ready: ready[what] || ready.document,
      context: { app: activeDesktopName || 'This Mac', detail: whatTitles[what] || 'Workspace' },
      tools: tools[what] || tools.document,
    };
  }

  businessNameInput?.addEventListener('input', () => {
    wizardAnswers.businessName = businessNameInput.value;
    if (!desktopNameInput?.dataset.touched || desktopNameInput.value === 'Work') {
      wizardAnswers.desktopName = businessNameInput.value.trim() || 'Work';
      if (desktopNameInput) desktopNameInput.value = wizardAnswers.desktopName;
    }
    refreshWizardControls();
  });
  desktopNameInput?.addEventListener('input', () => {
    desktopNameInput.dataset.touched = 'true';
    wizardAnswers.desktopName = desktopNameInput.value;
    refreshWizardControls();
  });
  document.querySelector('[data-unknown-business]')?.addEventListener('click', () => {
    wizardAnswers.businessName = '';
    wizardAnswers.desktopName = 'Work';
    if (businessNameInput) businessNameInput.value = '';
    if (desktopNameInput) {
      desktopNameInput.value = 'Work';
      delete desktopNameInput.dataset.touched;
    }
  });

  function saveWizardAnswers() {
    try {
      localStorage.setItem('mycelium.orientation.answers', JSON.stringify({
        ...wizardAnswers,
        desktop: buildDesktop(),
      }));
    } catch {}
  }

  function setWizardStep(step) {
    if (!onboarding) return;
    const max = onboarding.querySelectorAll('[data-wizard-step]').length;
    wizardStep = Math.min(max, Math.max(1, Number(step) || 1));
    onboarding.dataset.step = String(wizardStep);
    onboarding.querySelectorAll('[data-wizard-step]').forEach((node) => {
      node.hidden = Number(node.dataset.wizardStep) !== wizardStep;
    });
    onboarding.querySelectorAll('.onboarding-progress i').forEach((node, index) => {
      node.classList.toggle('is-active', index < wizardStep);
    });
    if (wizardStep === 6) renderConnections();
    syncNameFields();
    paintWizardChoices();
    refreshWizardControls();
    const focusable = onboarding.querySelector(
      `[data-wizard-step="${wizardStep}"] button:not(:disabled), [data-wizard-step="${wizardStep}"] input`,
    );
    focusable?.focus();
  }

  function openOrientation() {
    if (!onboarding) return;
    wizardAnswers = { where: null, what: null, desktopName: '', businessName: '' };
    if (desktopNameInput) { desktopNameInput.value = ''; delete desktopNameInput.dataset.touched; }
    if (businessNameInput) businessNameInput.value = '';
    paintWizardChoices();
    setWizardStep(1);
    onboarding.hidden = false;
  }
  function closeOrientation() {
    if (!onboarding || onboarding.hidden) return;
    onboarding.hidden = true;
    try { localStorage.setItem('mycelium.orientation.seen', '1'); } catch {}
  }
  try {
    if (localStorage.getItem('mycelium.orientation.seen') !== '1') {
      window.setTimeout(openOrientation, 700);
    }
  } catch {}

  setAgentContext('desk');
  void loadDesktops();

  const clock = document.getElementById('clock');
  function tick() {
    if (!clock) return;
    const now = new Date();
    clock.textContent = new Intl.DateTimeFormat('en-US', {
      weekday: 'short', hour: 'numeric', minute: '2-digit'
    }).format(now);
    clock.dateTime = now.toISOString();
  }
  tick();
  setInterval(tick, 30000);
})();
