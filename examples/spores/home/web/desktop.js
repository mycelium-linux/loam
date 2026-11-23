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
    if (url.hostname === 'files.myc') {
      if (activeDesktop) url.searchParams.set('path', activeDesktop);
    } else if (activeDesktop) {
      url.searchParams.set('desktop', activeDesktop);
    }
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
    const sporeButton = event.target.closest('[data-spore]');
    if (sporeButton) {
      event.preventDefault();
      void openSpore(sporeButton.dataset.spore);
      closeAccountMenu();
      closeCommand();
      return;
    }
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
      if (!wizardNext.disabled) setWizardStep(nextWizardStep(Number(onboarding?.dataset.step || 1)));
      return;
    }
    const wizardBack = event.target.closest('[data-wizard-back]');
    if (wizardBack) {
      event.preventDefault();
      setWizardStep(previousWizardStep(Number(onboarding?.dataset.step || 1)));
      return;
    }
    const wizardWhere = event.target.closest('[data-where]');
    if (wizardWhere) {
      event.preventDefault();
      if (wizardAnswers.where !== wizardWhere.dataset.where) {
        wizardAnswers.businessUnknown = false;
      }
      wizardAnswers.where = wizardWhere.dataset.where;
      syncNameFields();
      paintWizardChoices();
      refreshWizardControls();
      renderWizardCount();
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
    const wizardAi = event.target.closest('[data-ai]');
    if (wizardAi) {
      event.preventDefault();
      wizardAnswers.ai = wizardAi.dataset.ai;
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
  const wizardCount = document.getElementById('wizard-count');
  const businessField = document.getElementById('business-field');
  const businessNameInput = document.getElementById('business-name');
  let wizardStep = 1;
  let wizardAnswers = { where: null, what: null, businessName: '', businessUnknown: false, ai: null };

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
    if (businessField) businessField.hidden = wizardAnswers.where !== 'business';
    if (businessNameInput) businessNameInput.value = wizardAnswers.businessName || '';
  }

  function slug(value) {
    return value.trim().toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '') || 'work';
  }

  function buildDesktop() {
    const business = (wizardAnswers.businessName || '').trim();
    const isBusiness = wizardAnswers.where === 'business';
    const id = isBusiness ? slug(business || 'work') : 'personal';
    return {
      id,
      name: isBusiness ? (business || 'Work') : 'Personal',
      kind: isBusiness ? 'work' : 'personal',
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
      const selected = node.dataset.where === wizardAnswers.where;
      node.classList.toggle('is-selected', selected);
      node.setAttribute('aria-pressed', String(selected));
    });
    onboarding?.querySelectorAll('[data-what]').forEach((node) => {
      const selected = node.dataset.what === wizardAnswers.what;
      node.classList.toggle('is-selected', selected);
      node.setAttribute('aria-pressed', String(selected));
    });
    onboarding?.querySelectorAll('[data-ai]').forEach((node) => {
      const selected = node.dataset.ai === wizardAnswers.ai;
      node.classList.toggle('is-selected', selected);
      node.setAttribute('aria-pressed', String(selected));
    });
  }

  function refreshWizardControls() {
    const whereNext = onboarding?.querySelector('[data-wizard-step="2"] [data-wizard-next]');
    const businessNext = onboarding?.querySelector('[data-wizard-step="3"] [data-wizard-next]');
    const whatNext = onboarding?.querySelector('[data-wizard-step="4"] [data-wizard-next]');
    const start = onboarding?.querySelector('[data-wizard-step="5"] [data-wizard-start]');
    if (whereNext) whereNext.disabled = !wizardAnswers.where;
    if (businessNext) businessNext.disabled = !wizardAnswers.businessName.trim() && !wizardAnswers.businessUnknown;
    if (whatNext) whatNext.disabled = !wizardAnswers.what;
    if (start) start.disabled = !wizardAnswers.ai;
  }

  function nextWizardStep(step) {
    if (step === 2 && wizardAnswers.where !== 'business') return 4;
    return Math.min(5, step + 1);
  }

  function previousWizardStep(step) {
    if (step === 4 && wizardAnswers.where !== 'business') return 2;
    return Math.max(1, step - 1);
  }

  function renderWizardCount() {
    if (!wizardCount) return;
    const steps = wizardAnswers.where === 'business' ? [1, 2, 3, 4, 5] : [1, 2, 4, 5];
    const position = Math.max(1, steps.indexOf(wizardStep) + 1);
    wizardCount.textContent = `${position} / ${steps.length}`;
  }

  function setWizardStep(step) {
    if (!onboarding) return;
    let next = Math.min(5, Math.max(1, Number(step) || 1));
    if (next === 3 && wizardAnswers.where !== 'business') next = 4;
    wizardStep = next;
    onboarding.dataset.step = String(wizardStep);
    onboarding.querySelectorAll('[data-wizard-step]').forEach((node) => {
      node.hidden = Number(node.dataset.wizardStep) !== wizardStep;
    });
    syncNameFields();
    paintWizardChoices();
    refreshWizardControls();
    renderWizardCount();
    const focusable = onboarding.querySelector(
      `[data-wizard-step="${wizardStep}"] button:not(:disabled), [data-wizard-step="${wizardStep}"] input`,
    );
    focusable?.focus();
  }

  function openOrientation() {
    if (!onboarding) return;
    wizardAnswers = { where: null, what: null, businessName: '', businessUnknown: false, ai: null };
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

  function saveWizardAnswers() {
    try {
      localStorage.setItem('mycelium.orientation.answers', JSON.stringify({
        ...wizardAnswers,
        desktop: buildDesktop(),
      }));
    } catch {}
  }

  function buildAgentSeed() {
    const where = wizardAnswers.where || 'myself';
    const what = wizardAnswers.what || 'document';
    const ai = wizardAnswers.ai || 'later';
    const desktop = buildDesktop();
    const owner = where === 'business' ? 'my business' : 'myself';
    const prompts = {
      website: `Set up my first website for ${owner}. Start with the homepage.`,
      automation: 'Build my first automation. Start with one useful step.',
      agent: 'Build an agent that can work inside my apps. Start with one useful job.',
      presentation: 'Help me make my first presentation from what is already on this desk.',
      document: 'Start my first document and shape it from the workspace.',
    };
    const ready = {
      website: `I will start with the site files in ~/fungOS/${desktop.id}. No Git or cloud account is needed yet.`,
      automation: `I will start with the workflow in ~/fungOS/${desktop.id}. Add tools only when the first step needs them.`,
      agent: `I will start with Notes and the agent surface in ~/fungOS/${desktop.id}.`,
      presentation: `I will start with Notes and turn the workspace into an outline.`,
      document: `I will start with Notes and the VFS. The document stays local until you export it.`,
    };
    const aiTool = ai === 'fpl'
      ? { label: 'FPL AI', note: 'account link is next', state: 'waiting' }
      : ai === 'byo'
        ? { label: 'Your model', note: 'provider key is next', state: 'waiting' }
        : { label: 'AI provider', note: 'decided later', state: 'waiting' };
    const tools = {
      website: [
        { label: 'VFS', note: `~/fungOS/${desktop.id} site files`, state: 'preparing' },
        { label: 'Frontdoor', note: 'local preview', state: 'preparing' },
        aiTool,
        { label: 'Geode', note: 'credential vault', state: 'preparing' },
      ],
      automation: [
        { label: 'VFS', note: `~/fungOS/${desktop.id} workspace`, state: 'preparing' },
        { label: 'Loam', note: 'run the first step', state: 'preparing' },
        aiTool,
        { label: 'Geode', note: 'credential vault', state: 'preparing' },
      ],
      agent: [
        { label: 'Loam', note: 'agent harness', state: 'preparing' },
        { label: 'VFS', note: `~/fungOS/${desktop.id} notes`, state: 'preparing' },
        aiTool,
        { label: 'Geode', note: 'provider keys', state: 'preparing' },
      ],
      presentation: [
        { label: 'Notes', note: 'outline and slides', state: 'preparing' },
        { label: 'VFS', note: `~/fungOS/${desktop.id} source`, state: 'preparing' },
        aiTool,
        { label: 'Geode', note: 'credential vault', state: 'preparing' },
      ],
      document: [
        { label: 'Notes', note: 'long-form editor', state: 'preparing' },
        { label: 'VFS', note: `~/fungOS/${desktop.id} workspace`, state: 'preparing' },
        aiTool,
        { label: 'Geode', note: 'credential vault', state: 'preparing' },
      ],
    };
    const aiLine = ai === 'fpl'
      ? ' FPL account linking is the next wire.'
      : ai === 'byo'
        ? ' Your provider key goes into Geode next.'
        : '';
    return {
      prompt: prompts[what] || prompts.document,
      ready: `${ready[what] || ready.document}${aiLine}`,
      context: { app: activeDesktopName || 'This Mac', detail: whatTitles[what] || 'Workspace' },
      tools: tools[what] || tools.document,
      note: ai === 'fpl'
        ? 'Harness not connected yet — FPL AI and Geode are the next wires.'
        : ai === 'byo'
          ? 'Harness not connected yet — your provider key and Geode are the next wires.'
          : 'Harness not connected yet — add a model whenever you are ready.',
    };
  }

  businessNameInput?.addEventListener('input', () => {
    wizardAnswers.businessName = businessNameInput.value;
    wizardAnswers.businessUnknown = false;
    refreshWizardControls();
  });
  document.querySelector('[data-unknown-business]')?.addEventListener('click', () => {
    wizardAnswers.businessName = '';
    wizardAnswers.businessUnknown = true;
    if (businessNameInput) businessNameInput.value = '';
    refreshWizardControls();
  });

  // The launcher reads the host catalog instead of hard-coding a dock. The
  // bridge is the JSON face of `soil:spore/catalog`, so a Spore installed by
  // loamd appears here on the next load with no package edit.
  const catalog = new Map();

  function catalogKey(id) {
    return `spore-${id.replace(/[^a-z0-9]+/gi, '-').toLowerCase()}`;
  }

  function primarySurface(entry) {
    return entry.surfaces?.find((surface) => surface.name === entry.default_surface)
      || entry.surfaces?.find((surface) => surface.kind === 'web')
      || entry.surfaces?.[0]
      || null;
  }

  function catalogWindow(entry) {
    const key = catalogKey(entry.id);
    let el = windows.get(key);
    if (el) return el;
    el = document.createElement('section');
    el.className = 'window window--catalog';
    el.dataset.window = key;
    el.hidden = true;
    const offset = windows.size % 5;
    el.style.left = `calc(12vw + ${offset * 26}px)`;
    el.style.top = `calc(14vh + ${offset * 24}px)`;
    el.style.width = 'min(760px, 74vw)';
    el.style.height = 'min(560px, 68vh)';
    el.innerHTML = `
      <header class="window-bar">
        <div class="traffic">
          <button class="close" type="button" data-close="${key}" aria-label="Close"><span aria-hidden="true">×</span></button>
          <button class="minimize" type="button" data-minimize="${key}" aria-label="Minimize"><span aria-hidden="true">−</span></button>
          <button class="zoom" type="button" data-zoom="${key}" aria-label="Zoom"><span aria-hidden="true">+</span></button>
        </div>
        <h2 class="window-title"></h2>
        <span class="window-bar-spacer" aria-hidden="true"></span>
      </header>
      <iframe title="" loading="lazy"></iframe>
      <span class="window-resize" data-resize="${key}" aria-hidden="true"></span>`;
    el.querySelector('.window-title').textContent = entry.title || entry.name;
    el.querySelector('iframe').title = entry.title || entry.name;
    document.querySelector('main.desktop')?.append(el);
    windows.set(key, el);
    return el;
  }

  async function openSpore(id) {
    const entry = catalog.get(id);
    const surface = entry && primarySurface(entry);
    if (!surface) return;
    try {
      const response = await fetch('/api/spore/launch', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ id, surface: surface.name }),
      });
      const launched = await response.json();
      if (!response.ok || launched.error) {
        console.warn('spore launch failed', launched.error);
        return;
      }
      if (launched.kind === 'web') {
        const el = catalogWindow(entry);
        const frame = el.querySelector('iframe');
        if (frame && frame.getAttribute('src') !== launched.target) frame.src = launched.target;
        el.hidden = false;
        el.classList.add('is-open');
        focusWindow(el);
      }
    } catch (error) {
      console.warn('spore launch failed', error);
    }
  }

  async function loadCatalog() {
    try {
      const response = await fetch('/api/spore/entries', { headers: { accept: 'application/json' } });
      if (!response.ok) return;
      const body = await response.json();
      const entries = Array.isArray(body.entries) ? body.entries : [];
      const host = document.querySelector('.desktop-icons');
      if (!host) return;
      for (const entry of entries) {
        const surface = primarySurface(entry);
        if (!surface || entry.id === 'org.mycelium.home') continue;
        catalog.set(entry.id, entry);
        if (document.querySelector(`[data-spore="${CSS.escape(entry.id)}"]`)) continue;
        const button = document.createElement('button');
        button.className = 'desktop-icon catalog-icon';
        button.type = 'button';
        button.dataset.spore = entry.id;
        button.setAttribute('aria-label', `Open ${entry.title || entry.name}`);
        const tile = document.createElement('span');
        tile.className = 'app-tile';
        tile.style.setProperty('--app-bg', entry.color || '#8b9dff');
        tile.style.setProperty('--app-bg-2', entry.color || '#8b9dff');
        tile.style.setProperty('--app-ink', '#10141c');
        tile.textContent = entry.badge || (entry.title || entry.name || '?').slice(0, 1).toUpperCase();
        const label = document.createElement('span');
        label.textContent = entry.title || entry.name;
        button.append(tile, label);
        host.append(button);
      }
    } catch (error) {
      console.warn('launcher catalog unavailable', error);
    }
  }

  setAgentContext('desk');
  void loadDesktops();
  void loadCatalog();

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
