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
    const connection = event.target.closest('[data-connection]');
    if (connection) {
      event.preventDefault();
      const name = connection.dataset.connection;
      if (wizardConnections.has(name)) wizardConnections.delete(name);
      else wizardConnections.add(name);
      paintConnections();
      return;
    }
    const wizardStart = event.target.closest('[data-wizard-start]');
    if (wizardStart) {
      event.preventDefault();
      saveWizardAnswers();
      closeOrientation();
      open(wizardAnswers.what === 'website' ? 'files' : 'notes');
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
  let wizardConnections = new Set();

  const connectionCatalog = {
    website: {
      myself: [['Cloudflare', 'Domains, DNS, and the public edge'], ['GitHub', 'The code behind the site'], ['Stripe', 'Payments when you need them'], ['Plausible', 'Quiet analytics']],
      business: [['Cloudflare', 'Domains, DNS, and the public edge'], ['GitHub', 'Site code and review'], ['Stripe', 'Checkout and billing'], ['Linear', 'The launch plan']],
    },
    automation: {
      myself: [['GitHub', 'Repositories and triggers'], ['Slack', 'Notifications where you already are'], ['Google Drive', 'The files the flow touches'], ['Home Assistant', 'Your local devices']],
      business: [['GitHub', 'Repositories and review'], ['Slack', 'Team notifications'], ['Linear', 'Work items and handoffs'], ['Google Workspace', 'Mail, calendar, and files']],
    },
    agent: {
      myself: [['OpenAI', 'A model to reason with'], ['Anthropic', 'A second model'], ['GitHub', 'The code it can change'], ['Slack', 'A place to report back']],
      business: [['OpenAI', 'Team model access'], ['Anthropic', 'A second provider'], ['GitHub', 'Code and review'], ['Slack', 'Team context']],
    },
    presentation: {
      myself: [['Figma', 'The visual source'], ['Google Drive', 'Slides and files'], ['Notion', 'The outline'], ['Loom', 'A quick recording']],
      business: [['Figma', 'Brand and layout'], ['Google Drive', 'Shared decks'], ['Notion', 'The narrative'], ['Slack', 'Review and feedback']],
    },
    document: {
      myself: [['Notion', 'Long-form writing'], ['Google Drive', 'Files and exports'], ['Dropbox', 'The archive'], ['Readwise', 'The reading trail']],
      business: [['Notion', 'The team wiki'], ['Google Drive', 'Shared documents'], ['Dropbox', 'The archive'], ['Linear', 'The work it belongs to']],
    },
  };

  const whatLabels = {
    website: 'a website',
    automation: 'an automation',
    agent: 'an agent',
    presentation: 'a presentation',
    document: 'a document',
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

  function paintConnections() {
    if (!connectionsList) return;
    connectionsList.querySelectorAll('[data-connection]').forEach((node) => {
      const selected = wizardConnections.has(node.dataset.connection);
      node.classList.toggle('is-selected', selected);
      node.setAttribute('aria-pressed', String(selected));
    });
  }

  function renderConnections() {
    if (!connectionsList) return;
    const where = wizardAnswers.where || 'myself';
    const what = wizardAnswers.what || 'document';
    const options = connectionCatalog[what]?.[where] || [];
    wizardConnections = new Set(options.map(([name]) => name));
    connectionsList.replaceChildren();
    if (connectionsTitle) connectionsTitle.textContent = 'Start with these.';
    if (connectionsCopy) {
      connectionsCopy.textContent = `For ${where === 'business' ? 'a business' : 'yourself'}, starting with ${whatLabels[what]}, these are the first connections we would prioritize. You can change them later.`;
    }
    for (const [name, note] of options) {
      const button = document.createElement('button');
      button.className = 'connection';
      button.type = 'button';
      button.dataset.connection = name;
      button.setAttribute('aria-pressed', 'true');
      const mark = document.createElement('span');
      mark.className = 'connection-mark';
      mark.textContent = name.slice(0, 1);
      const copy = document.createElement('span');
      copy.className = 'connection-copy';
      const strong = document.createElement('strong');
      strong.textContent = name;
      const small = document.createElement('small');
      small.textContent = note;
      copy.append(strong, small);
      const check = document.createElement('span');
      check.className = 'connection-check';
      check.textContent = '✓';
      button.append(mark, copy, check);
      connectionsList.append(button);
    }
    paintConnections();
  }

  function saveWizardAnswers() {
    try {
      localStorage.setItem('mycelium.orientation.answers', JSON.stringify({
        ...wizardAnswers,
        connections: [...wizardConnections],
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
    wizardConnections = new Set();
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
