(() => {
  const root = document.documentElement;
  const windows = new Map();
  let z = 10;

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

  function open(kind) {
    const el = windows.get(kind);
    if (!el) return;
    el.hidden = false;
    el.classList.add('is-open');
    const frame = el.querySelector('iframe[data-src]');
    if (frame && frame.getAttribute('src') !== frame.dataset.src) {
      frame.src = frame.dataset.src;
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
      wizardAnswers.where = wizardWhere.dataset.where;
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
      saveWizardAnswers();
      closeOrientation();
      open(wizardAnswers.what === 'website' ? 'files' : 'notes');
      window.setTimeout(() => window.MyceliumAgent?.seed(buildAgentSeed()), 420);
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

  fetch('/api/vfs/roots')
    .then((response) => response.ok ? response.json() : null)
    .then((data) => {
      const vfs = data?.roots?.find((root) => root.id === 'vfs');
      const path = vfs?.mount || vfs?.root;
      if (path && accountWorkspace) {
        accountWorkspace.textContent = path.replace(/^\/Users\/[^/]+/, '~');
      }
    })
    .catch(() => {});

  const onboarding = document.getElementById('onboarding');
  const connectionsTitle = document.getElementById('connections-title');
  const connectionsCopy = document.getElementById('connections-copy');
  const connectionsList = document.getElementById('connections-list');
  let wizardStep = 1;
  let wizardAnswers = { where: null, what: null };

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
    if (whereNext) whereNext.disabled = !wizardAnswers.where;
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
        { label: 'VFS', note: '~/fungOS site files', state: 'preparing' },
        { label: 'Frontdoor', note: 'local preview', state: 'preparing' },
        { label: 'FPL Auth', note: 'Cloudflare · GitHub', state: 'waiting' },
        { label: 'Geode', note: 'credential vault', state: 'preparing' },
      ],
      automation: [
        { label: 'VFS', note: 'workspace and triggers', state: 'preparing' },
        { label: 'Loam', note: 'run the first step', state: 'preparing' },
        { label: 'FPL Auth', note: 'Google · GitHub', state: 'waiting' },
        { label: 'Geode', note: 'credential vault', state: 'preparing' },
      ],
      agent: [
        { label: 'Loam', note: 'agent harness', state: 'preparing' },
        { label: 'VFS', note: 'notes and files', state: 'preparing' },
        { label: 'FPL Auth', note: 'OpenAI · xAI', state: 'waiting' },
        { label: 'Geode', note: 'provider keys', state: 'preparing' },
      ],
      presentation: [
        { label: 'Notes', note: 'outline and slides', state: 'preparing' },
        { label: 'VFS', note: 'source material', state: 'preparing' },
        { label: 'FPL Auth', note: 'Google', state: 'waiting' },
        { label: 'Geode', note: 'credential vault', state: 'preparing' },
      ],
      document: [
        { label: 'Notes', note: 'long-form editor', state: 'preparing' },
        { label: 'VFS', note: 'workspace', state: 'preparing' },
        { label: 'FPL Auth', note: 'Google', state: 'waiting' },
        { label: 'Geode', note: 'credential vault', state: 'preparing' },
      ],
    };
    return {
      prompt: prompts[what] || prompts.document,
      ready: ready[what] || ready.document,
      context: { app: 'This Mac', detail: whatTitles[what] || 'Workspace' },
      tools: tools[what] || tools.document,
    };
  }

  function saveWizardAnswers() {
    try {
      localStorage.setItem('mycelium.orientation.answers', JSON.stringify({
        ...wizardAnswers,
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
    paintWizardChoices();
    refreshWizardControls();
    const focusable = onboarding.querySelector(
      `[data-wizard-step="${wizardStep}"] button:not(:disabled), [data-wizard-step="${wizardStep}"] input`,
    );
    focusable?.focus();
  }

  function openOrientation() {
    if (!onboarding) return;
    wizardAnswers = { where: null, what: null };
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
