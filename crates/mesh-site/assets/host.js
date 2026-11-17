// SocialMesh app host (SPEC.md §11.2): the only script that runs in an app's
// page. It starts the app's worker, shows what the app renders, passes
// clicks and form submissions back, and carries out the app's few requests
// through the viewer (the phone app, or the desktop viewer's local API).
(function () {
  'use strict';
  const me = document.currentScript;
  const root = me.getAttribute('data-root') || '';
  const appPath = me.getAttribute('data-app');

  // The same neutralising as site pages (mesh-site view.rs): nothing an app
  // renders can redirect the page, make the browser contact a server, or
  // open a frame (browsers preconnect to a frame's address before refusing it).
  const WORDS = [
    ['http-equiv', 'data-equiv'],
    ['preconnect', 'xreconnect'],
    ['dns-prefetch', 'dns-xrefetch'],
    ['prerender', 'xrerender'],
    ['<iframe', '<xframe'],
    ['<frame', '<xrame'],
    ['<object', '<xbject'],
    ['<embed', '<xmbed'],
    ['<portal', '<xortal'],
    ['<fencedframe', '<xencedframe'],
  ];
  function neutralise(html) {
    WORDS.forEach(function (w) {
      html = html.replace(new RegExp(w[0], 'gi'), w[1]);
    });
    return html;
  }

  function nativeBridge(n) {
    const waiting = new Map();
    let next = 1;
    window.__socialmeshDone = function (id, json) {
      const p = waiting.get(id);
      if (!p) return;
      waiting.delete(id);
      const r = JSON.parse(json);
      if (r.ok) p.resolve(r.id);
      else p.reject(new Error(r.error));
    };
    return {
      app: function () {
        return Promise.resolve(JSON.parse(n.app()));
      },
      entries: function () {
        return Promise.resolve(JSON.parse(n.entries()));
      },
      submit: function (data, summary) {
        return new Promise(function (resolve, reject) {
          const id = next++;
          waiting.set(id, { resolve: resolve, reject: reject });
          n.submit(id, data, summary);
        });
      },
    };
  }

  function httpBridge() {
    function json(r) {
      return r.json().then(function (v) {
        if (v && v.error) throw new Error(v.error);
        return v;
      });
    }
    return {
      app: function () {
        return fetch(root + '.socialmesh/api/app').then(json);
      },
      entries: function () {
        return fetch(root + '.socialmesh/api/entries').then(json);
      },
      submit: function (data, summary) {
        return fetch(root + '.socialmesh/api/submit', {
          method: 'POST',
          body: JSON.stringify({ data: data, summary: summary }),
        })
          .then(json)
          .then(function (v) {
            return v.id;
          });
      },
    };
  }

  function start() {
    const area = document.querySelector('[data-socialmesh-app]');
    if (!area || !appPath) return;
    const bridge = window.SocialMeshNative ? nativeBridge(window.SocialMeshNative) : httpBridge();
    const worker = new Worker(root + '.socialmesh/worker.js');
    const storageKey = function (k) {
      return 'app:' + k;
    };

    worker.onmessage = function (e) {
      const m = e.data;
      if (m.type === 'render') {
        area.innerHTML = neutralise(String(m.html));
        return;
      }
      const reply = function (ok, value, error) {
        worker.postMessage({ type: 'reply', id: m.id, ok: ok, value: value, error: error });
      };
      let p;
      if (m.type === 'entries') p = bridge.entries();
      else if (m.type === 'submit') p = bridge.submit(String(m.data), String(m.summary));
      else if (m.type === 'storage.get') p = Promise.resolve(localStorage.getItem(storageKey(m.key)));
      else if (m.type === 'storage.set') {
        localStorage.setItem(storageKey(m.key), String(m.value));
        p = Promise.resolve(null);
      } else return;
      p.then(
        function (v) {
          reply(true, v);
        },
        function (err) {
          reply(false, null, String((err && err.message) || err));
        },
      );
    };

    area.addEventListener('click', function (ev) {
      const t = ev.target.closest ? ev.target.closest('[data-action]') : null;
      if (!t || !area.contains(t)) return;
      ev.preventDefault();
      worker.postMessage({
        type: 'event',
        kind: 'click',
        action: t.getAttribute('data-action'),
        value: t.getAttribute('data-value'),
      });
    });
    area.addEventListener('submit', function (ev) {
      ev.preventDefault();
      const values = {};
      new FormData(ev.target).forEach(function (v, k) {
        values[k] = String(v);
      });
      worker.postMessage({ type: 'event', kind: 'submit', action: ev.target.getAttribute('data-action'), values: values });
    });
    area.addEventListener('change', function (ev) {
      const t = ev.target;
      worker.postMessage({
        type: 'event',
        kind: 'change',
        action: t.getAttribute('data-action'),
        name: t.name || null,
        value: t.type === 'checkbox' ? String(t.checked) : String(t.value),
      });
    });

    bridge.app().then(function (app) {
      worker.postMessage({ type: 'start', script: new URL(root + appPath, location.href).href, app: app });
    });
  }

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', start);
  else start();
})();
