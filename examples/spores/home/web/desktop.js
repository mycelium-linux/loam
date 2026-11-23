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

  function focusWindow(el) {
    el.style.zIndex = String(++z);
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
      setWizardStep(Number(onboarding?.dataset.step || 1) + 1);
      return;
    }
    const wizardBack = event.target.closest('[data-wizard-back]');
    if (wizardBack) {
      event.preventDefault();
      setWizardStep(Number(onboarding?.dataset.step || 1) - 1);
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
  let wizardStep = 1;

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
    const focusable = onboarding.querySelector(
      `[data-wizard-step="${wizardStep}"] button, [data-wizard-step="${wizardStep}"] input`,
    );
    focusable?.focus();
  }

  function openOrientation() {
    if (!onboarding) return;
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
