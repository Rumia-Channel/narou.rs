/**
 * Dropdown open/close handler (pure JS replacement for Bootstrap dropdown)
 */

let openDropdown = null;

export function initDropdowns() {
  document.addEventListener('click', handleClick, true);
  window.addEventListener('resize', () => {
    if (openDropdown) fitDropdown(openDropdown.querySelector('.dropdown-menu'), openDropdown);
  });
}

function handleClick(e) {
  if (e.target.closest('#select-color-menu')) {
    return;
  }

  const toggle = e.target.closest('[data-dropdown]');

  if (toggle) {
    e.preventDefault();
    e.stopPropagation();

    const menuId = toggle.getAttribute('data-dropdown');
    const menu = document.getElementById(menuId);
    if (!menu) return;

    const parent = toggle.closest('.dropdown, .btn-group, li');
    if (!parent) return;

    if (parent.classList.contains('open')) {
      closeAll();
    } else {
      closeAll();
      parent.classList.add('open');
      openDropdown = parent;
      fitDropdown(menu, parent);
    }
    return;
  }

  // Click inside an open menu item (a link) — close after action
  if (e.target.closest('.dropdown-menu a')) {
    setTimeout(closeAll, 50);
    return;
  }

  // Click outside — close
  if (openDropdown && !e.target.closest('.dropdown-menu')) {
    closeAll();
  }
}

function fitDropdown(menu, parent) {
  if (!menu) return;
  menu.style.removeProperty('--dropdown-available-height');
  menu.style.removeProperty('--dropdown-offset-x');
  if (getComputedStyle(menu).position === 'static') return;
  const rootSize = Number.parseFloat(getComputedStyle(document.documentElement).fontSize) || 16;
  const upward = Boolean(parent.closest('.navbar-fixed-bottom'));
  const rect = parent.getBoundingClientRect();
  const available = upward ? rect.top : window.innerHeight - rect.bottom;
  menu.style.setProperty('--dropdown-available-height', Math.max(0, available / rootSize - 1) + 'rem');
  menu.style.setProperty('--dropdown-offset-x', '0rem');
  const bounds = menu.getBoundingClientRect();
  const gutter = rootSize / 2;
  const offset = bounds.right > window.innerWidth - gutter
    ? window.innerWidth - gutter - bounds.right
    : bounds.left < gutter ? gutter - bounds.left : 0;
  menu.style.setProperty('--dropdown-offset-x', offset / rootSize + 'rem');
}

function closeAll() {
  if (openDropdown) {
    openDropdown.classList.remove('open');
    openDropdown = null;
  }
  document.querySelectorAll('.dropdown.open, .btn-group.open, li.open').forEach(el => el.classList.remove('open'));
}
