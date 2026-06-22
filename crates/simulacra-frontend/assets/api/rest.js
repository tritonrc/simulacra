// rest.js — fetch wrappers for /api/v1/*.
// `restJson` for JSON request/response; `restMultipart` for file uploads.

export async function restJson(path, opts = {}) {
  const headers = { ...(opts.headers ?? {}) };
  let body = opts.body;
  if (body !== undefined && typeof body !== 'string' && !(body instanceof FormData)) {
    headers['content-type'] = 'application/json';
    body = JSON.stringify(body);
  }
  const response = await fetch(path, {
    method: opts.method ?? 'GET',
    headers,
    body,
  });
  if (!response.ok) {
    throw new Error(`REST ${path} → HTTP ${response.status}`);
  }
  const ct = response.headers.get('content-type') ?? '';
  if (ct.startsWith('application/json')) {
    return await response.json();
  }
  return await response.text();
}

export async function restMultipart(path, formData) {
  const response = await fetch(path, { method: 'POST', body: formData });
  if (!response.ok) {
    throw new Error(`REST ${path} → HTTP ${response.status}`);
  }
  return await response.json();
}
