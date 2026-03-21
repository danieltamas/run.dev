// rundev — Node.js preload script
// Intercepts .env file reads and merges in secrets from process environment.
// Secrets never touch disk; they live only in process memory.
(function () {
  'use strict';
  const fs = require('fs');
  const path = require('path');

  const raw = process.env.__RUNDEV_ENV;
  if (!raw) return;

  let injected;
  try {
    injected = JSON.parse(raw);
  } catch {
    return;
  }

  // Remove the JSON blob so the app never sees it
  delete process.env.__RUNDEV_ENV;

  if (!Object.keys(injected).length) return;

  function isEnvFile(p) {
    if (!p) return false;
    const name = path.basename(String(p));
    return name === '.env' || name.startsWith('.env.');
  }

  // Merge injected vars into .env content.
  // Existing keys are overridden; new keys are appended.
  function mergeEnv(content) {
    const lines = content.split('\n');
    const remaining = Object.assign({}, injected);
    const result = [];

    for (const line of lines) {
      const trimmed = line.trim();
      if (trimmed && !trimmed.startsWith('#')) {
        const eq = trimmed.indexOf('=');
        if (eq > 0) {
          const key = trimmed.substring(0, eq).trim();
          if (key in remaining) {
            result.push(key + '=' + remaining[key]);
            delete remaining[key];
            continue;
          }
        }
      }
      result.push(line);
    }

    for (const k of Object.keys(remaining)) {
      result.push(k + '=' + remaining[k]);
    }

    return result.join('\n');
  }

  function patchResult(original, merged) {
    return typeof original === 'string' ? merged : Buffer.from(merged, 'utf8');
  }

  // --- readFileSync ---
  const _readFileSync = fs.readFileSync;
  fs.readFileSync = function (p, opts) {
    const result = _readFileSync.call(this, p, opts);
    if (isEnvFile(p)) {
      const str = typeof result === 'string' ? result : result.toString('utf8');
      return patchResult(result, mergeEnv(str));
    }
    return result;
  };

  // --- readFile (callback) ---
  const _readFile = fs.readFile;
  fs.readFile = function (p) {
    const args = Array.prototype.slice.call(arguments, 1);
    const cb = typeof args[args.length - 1] === 'function' ? args.pop() : null;

    if (cb && isEnvFile(p)) {
      const wrappedCb = function (err, data) {
        if (err) return cb(err);
        const str = typeof data === 'string' ? data : data.toString('utf8');
        cb(null, patchResult(data, mergeEnv(str)));
      };
      args.push(wrappedCb);
      return _readFile.apply(this, [p].concat(args));
    }

    if (cb) args.push(cb);
    return _readFile.apply(this, [p].concat(args));
  };

  // --- fs.promises.readFile ---
  var _promisesReadFile = fs.promises.readFile;
  fs.promises.readFile = function (p, opts) {
    return _promisesReadFile.call(this, p, opts).then(function (result) {
      if (isEnvFile(p)) {
        var str = typeof result === 'string' ? result : result.toString('utf8');
        return patchResult(result, mergeEnv(str));
      }
      return result;
    });
  };
})();
