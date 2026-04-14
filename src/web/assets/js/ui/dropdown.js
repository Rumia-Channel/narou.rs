/**
 * Dropdown open/close handler (pure JS replacement for Bootstrap dropdown)
 */

let openDropdown = null;

export function initDropdowns() {
  document.addEventListener('click', handleClick, true);
}

function handleClick(e) {
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

function closeAll() {
  if (openDropdown) {
    openDropdown.classList.remove('open');
    openDropdown = null;
  }
  document.querySelectorAll('.open').forEach(el => el.classList.remove('open'));
}
