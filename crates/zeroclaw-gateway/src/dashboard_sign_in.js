// The server embeds localized text and checked route settings. Association
// cookies are noncredentials and have exactly one writer: this synchronous
// assignment while the origin/path's Web Lock is held. HTTP never writes them.
(async () => {
  const title = document.getElementById('title');
  const message = document.getElementById('message');
  const retry = document.getElementById('retry');
  const text = settings.text;
  function show(value) { document.title = value; title.textContent = value; }
  function interrupted(value = text.interrupted) {
    show(text.interrupted_title); message.textContent = value;
    const url = new URL(`${settings.prefix}/auth/sign-in/${encodeURIComponent(settings.alias)}`, location.origin);
    url.searchParams.set('return', settings.return_to);
    if (settings.popup) url.searchParams.set('popup', '1');
    retry.href = url.href; retry.textContent = text.retry; retry.hidden = false;
  }
  async function request(path, body, csrf) {
    const headers = { 'X-ZeroClaw-Browser': '1' };
    if (csrf) headers['X-ZeroClaw-CSRF'] = csrf;
    if (body !== undefined) headers['Content-Type'] = 'application/json';
    const response = await fetch(`${settings.prefix}${path}`, {
      method: body === undefined ? 'GET' : 'POST', cache: 'no-store', credentials: 'same-origin',
      headers, ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    if (!response.ok) throw new Error('browser_context_changed');
    return response.status === 204 ? null : response.json();
  }
  if (settings.mode === 'interrupted') { interrupted(); return; }
  if (settings.mode === 'complete') {
    show(text.checking);
    let channel;
    try {
      // A probe is a snapshot at the server lookup. Observe cross-tab changes
      // before issuing it so a held response cannot contradict a replacement
      // or logout already announced while the request was pending.
      let observed = 0;
      try {
        channel = new BroadcastChannel('zeroclaw-auth');
        channel.onmessage = event => {
          if (event.data?.type === 'signed-in' || event.data?.type === 'signed-out') observed += 1;
        };
      } catch { /* Without notifications, the server probe remains a snapshot. */ }
      for (let attempt = 0; attempt < 3; attempt += 1) {
        const before = observed;
        let current;
        try { current = await request('/api/auth/session'); }
        catch (error) { if (observed !== before) continue; throw error; }
        if (observed !== before) continue;
        if (current.kind !== 'oidc' || current.session_tag !== settings.session_tag) {
          interrupted(); return;
        }
        show(text.complete); message.textContent = current.display_name || current.principal_id;
        try { channel?.postMessage({ type: 'signed-in' }); }
        catch { /* Other tabs retain their existing focus recheck. */ }
        if (settings.popup) { message.append(document.createTextNode(` ${text.close}`)); window.close(); }
        else location.replace(`${settings.prefix}${settings.return_to}`);
        return;
      }
      interrupted();
    } catch { interrupted(); }
    finally { channel?.close(); }
    return;
  }
  show(text.prepare);
  if (!navigator.locks) { interrupted(text.storage); return; }
  try {
    let browser, csrf;
    await navigator.locks.request(`zeroclaw-browser:${settings.prefix}`, async () => {
      const context = await request('/api/auth/browser');
      csrf = context.session_csrf;
      browser = await request('/api/auth/browser', context, csrf);
      // Do not move this write to a server Set-Cookie response: an abandoned
      // request can otherwise overwrite a later tab's established association.
      document.cookie = `${settings.cookie_name}=${browser.nonce}${settings.cookie_suffix}`;
      await request('/api/auth/browser/confirm', browser, csrf);
    });
    const query = new URLSearchParams({ return: settings.return_to });
    if (settings.popup) query.set('popup', '1');
    const started = await request(`/auth/sign-in/${encodeURIComponent(settings.alias)}?${query}`, browser, csrf);
    location.replace(started.authorize_url);
  } catch { interrupted(); }
})();
