async function prepareBrowserToken(functionPath, scriptUrl, expectedOrigin) {
  if (location.href === 'about:blank') return { stage: 'context_pending' };
  if (location.origin !== expectedOrigin) return { stage: 'wrong_origin' };
  if (document.readyState === 'loading') {
    await new Promise(resolve => {
      document.addEventListener('DOMContentLoaded', resolve, { once: true });
    });
  }
  if (location.origin !== expectedOrigin) return { stage: 'wrong_origin' };
  const segments = functionPath.split('.');
  function resolve() {
    let owner = globalThis;
    for (let index = 0; index < segments.length - 1; index += 1) {
      if (owner == null) return null;
      owner = owner[segments[index]];
    }
    if (owner == null) return null;
    const fn = owner[segments[segments.length - 1]];
    return typeof fn === 'function' ? { owner, fn } : null;
  }
  let callable = resolve();
  if (!callable) {
    const script = document.createElement('script');
    script.src = scriptUrl;
    script.async = false;
    const loaded = await new Promise(resolve => {
      script.onload = () => resolve(true);
      script.onerror = () => resolve(false);
      (document.head || document.documentElement).appendChild(script);
    });
    if (!loaded) return { stage: 'script_failed' };
    callable = resolve();
    if (!callable) return { stage: 'missing' };
  }
  await callable.fn.apply(callable.owner, []);
  return { stage: 'ok' };
}
