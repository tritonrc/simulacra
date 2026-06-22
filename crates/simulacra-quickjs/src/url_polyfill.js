// URL and URLSearchParams polyfill for QuickJS.
// Handles http/https URLs, query strings, hash fragments.
// Not a full WHATWG parser — sufficient for agent use cases.

(() => {
    class URLSearchParams {
        #params;

        constructor(init) {
            this.#params = [];
            if (typeof init === 'string') {
                const str = init.startsWith('?') ? init.slice(1) : init;
                if (str) {
                    for (const pair of str.split('&')) {
                        const idx = pair.indexOf('=');
                        if (idx === -1) {
                            this.#params.push([decodeURIComponent(pair), '']);
                        } else {
                            this.#params.push([
                                decodeURIComponent(pair.slice(0, idx)),
                                decodeURIComponent(pair.slice(idx + 1))
                            ]);
                        }
                    }
                }
            } else if (Array.isArray(init)) {
                for (const [k, v] of init) {
                    this.#params.push([String(k), String(v)]);
                }
            } else if (init && typeof init === 'object') {
                for (const k of Object.keys(init)) {
                    this.#params.push([k, String(init[k])]);
                }
            }
        }

        get(name) {
            const entry = this.#params.find(([k]) => k === name);
            return entry ? entry[1] : null;
        }

        getAll(name) {
            return this.#params.filter(([k]) => k === name).map(([, v]) => v);
        }

        set(name, value) {
            let found = false;
            this.#params = this.#params.filter(([k]) => {
                if (k === name) {
                    if (!found) { found = true; return true; }
                    return false;
                }
                return true;
            });
            if (found) {
                const entry = this.#params.find(([k]) => k === name);
                if (entry) entry[1] = String(value);
            } else {
                this.#params.push([name, String(value)]);
            }
        }

        append(name, value) {
            this.#params.push([String(name), String(value)]);
        }

        delete(name) {
            this.#params = this.#params.filter(([k]) => k !== name);
        }

        has(name) {
            return this.#params.some(([k]) => k === name);
        }

        toString() {
            return this.#params
                .map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(v)}`)
                .join('&');
        }

        entries() { return this.#params[Symbol.iterator](); }
        keys() { return this.#params.map(([k]) => k)[Symbol.iterator](); }
        values() { return this.#params.map(([, v]) => v)[Symbol.iterator](); }

        forEach(callback, thisArg) {
            for (const [k, v] of this.#params) {
                callback.call(thisArg, v, k, this);
            }
        }

        [Symbol.iterator]() { return this.entries(); }
    }

    class URL {
        #protocol = '';
        #username = '';
        #password = '';
        #hostname = '';
        #port = '';
        #pathname = '/';
        #search = '';
        #hash = '';
        #searchParams;

        constructor(url, base) {
            let href = String(url);

            // If relative and base is provided, resolve against base
            if (base !== undefined) {
                const baseUrl = new URL(String(base));
                if (href.startsWith('//')) {
                    href = baseUrl.protocol + href;
                } else if (href.startsWith('/')) {
                    href = baseUrl.origin + href;
                } else if (!/^[a-zA-Z][a-zA-Z0-9+\-.]*:/.test(href)) {
                    // Relative path
                    const basePath = baseUrl.pathname.replace(/\/[^/]*$/, '/');
                    href = baseUrl.origin + basePath + href;
                }
            }

            // Parse: protocol://username:password@hostname:port/pathname?search#hash
            const match = href.match(/^([a-zA-Z][a-zA-Z0-9+\-.]*:)\/\/(?:([^:@]*)(?::([^@]*))?@)?([^/:?#]*)(?::(\d+))?(\/[^?#]*)?(\?[^#]*)?(#.*)?$/);
            if (!match) {
                throw new TypeError(`Invalid URL: ${url}`);
            }

            this.#protocol = match[1] || '';
            this.#username = match[2] ? decodeURIComponent(match[2]) : '';
            this.#password = match[3] ? decodeURIComponent(match[3]) : '';
            this.#hostname = match[4] || '';
            this.#port = match[5] || '';
            this.#pathname = match[6] || '/';
            this.#search = match[7] || '';
            this.#hash = match[8] || '';
            this.#searchParams = new URLSearchParams(this.#search);
        }

        get protocol() { return this.#protocol; }
        set protocol(v) { this.#protocol = v.endsWith(':') ? v : v + ':'; }

        get username() { return this.#username; }
        set username(v) { this.#username = v; }

        get password() { return this.#password; }
        set password(v) { this.#password = v; }

        get hostname() { return this.#hostname; }
        set hostname(v) { this.#hostname = v; }

        get port() { return this.#port; }
        set port(v) { this.#port = String(v); }

        get pathname() { return this.#pathname; }
        set pathname(v) { this.#pathname = v.startsWith('/') ? v : '/' + v; }

        get search() { return this.#search; }
        set search(v) { this.#search = v.startsWith('?') ? v : (v ? '?' + v : ''); this.#searchParams = new URLSearchParams(this.#search); }

        get hash() { return this.#hash; }
        set hash(v) { this.#hash = v.startsWith('#') ? v : (v ? '#' + v : ''); }

        get host() {
            return this.#port ? `${this.#hostname}:${this.#port}` : this.#hostname;
        }

        get origin() {
            return `${this.#protocol}//${this.host}`;
        }

        get searchParams() { return this.#searchParams; }

        get href() {
            let auth = '';
            if (this.#username) {
                auth = this.#password
                    ? `${encodeURIComponent(this.#username)}:${encodeURIComponent(this.#password)}@`
                    : `${encodeURIComponent(this.#username)}@`;
            }
            return `${this.#protocol}//${auth}${this.host}${this.#pathname}${this.#search}${this.#hash}`;
        }

        toString() { return this.href; }
        toJSON() { return this.href; }
    }

    globalThis.URL = URL;
    globalThis.URLSearchParams = URLSearchParams;
})();
