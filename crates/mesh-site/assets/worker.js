// SocialMesh app worker bootstrap (SPEC.md §11.2).
//
// App code runs here, in a worker: no page, no WebRTC, and a policy that
// allows no connections. It reaches the outside world only through the
// requests below, which the viewer's host script answers.
'use strict';
(function () {
  const pending = new Map();
  const handlers = {};
  let next = 1;

  function call(type, payload) {
    return new Promise(function (resolve, reject) {
      const id = next++;
      pending.set(id, { resolve: resolve, reject: reject });
      postMessage(Object.assign({ type: type, id: id }, payload));
    });
  }

  function fire(kind, arg) {
    (handlers[kind] || []).forEach(function (f) {
      try {
        f(arg);
      } catch (err) {
        console.error(err);
      }
    });
  }

  self.socialmesh = {
    /** About this app: { title, address, version }. Set before your script runs. */
    app: null,
    /** Show `html` in the app's area of the page. Scripts in it never run. */
    render: function (html) {
      postMessage({ type: 'render', html: String(html) });
    },
    /** Every entry sent to this app: [{ id, author, mine, created_ms, data }], oldest first. */
    entries: function () {
      return call('entries', {});
    },
    /**
     * Ask the person to publish `data` (a string, or anything JSON can hold) as an entry.
     * They see `summary` and the exact data, and choose who to sign as. Resolves with the
     * entry's id; rejects if they decline.
     */
    submit: function (data, summary) {
      const text = typeof data === 'string' ? data : JSON.stringify(data);
      return call('submit', { data: text, summary: String(summary || '') });
    },
    /** A small store for this app on this device, never shared. */
    storage: {
      get: function (key) {
        return call('storage.get', { key: String(key) });
      },
      set: function (key, value) {
        return call('storage.set', { key: String(key), value: String(value) });
      },
    },
    /** Listen for 'start', 'click', 'submit' or 'change'. */
    on: function (kind, f) {
      (handlers[kind] = handlers[kind] || []).push(f);
    },
  };

  onmessage = function (e) {
    const m = e.data;
    if (m.type === 'reply') {
      const p = pending.get(m.id);
      if (!p) return;
      pending.delete(m.id);
      if (m.ok) p.resolve(m.value);
      else p.reject(new Error(m.error));
    } else if (m.type === 'start') {
      self.socialmesh.app = m.app;
      importScripts(m.script);
      fire('start', m.app);
    } else if (m.type === 'event') {
      fire(m.kind, m);
    }
  };
})();
