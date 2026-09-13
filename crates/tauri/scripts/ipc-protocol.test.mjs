// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

import assert from 'node:assert/strict'
import { webcrypto } from 'node:crypto'
import { readFileSync } from 'node:fs'
import test from 'node:test'
import vm from 'node:vm'

const readScript = (name) => readFileSync(new URL(name, import.meta.url), 'utf8')
const protocolSource = readScript('ipc-protocol.js')
const coreSource = readScript('core.js')
const processMessageSource = readScript('process-ipc-message-fn.js')
const transportError = /IPC custom protocol request failed.*not retried/

function runtime(fetchImplementation, { nativePostMessage, ipcValue } = {}) {
  const requests = []
  const nativeMessages = []
  const window = { __TAURI_INTERNALS__: {}, crypto: webcrypto }
  if (ipcValue !== undefined) window.ipc = ipcValue
  if (nativePostMessage) {
    window.ipc = {
      postMessage(data) {
        const message = JSON.parse(data)
        nativeMessages.push(message)
        nativePostMessage(message, window.__TAURI_INTERNALS__)
      }
    }
  }
  const context = vm.createContext({
    window, Headers, ArrayBuffer, Uint8Array, Uint32Array, Map, setTimeout,
    console: { warn() {} },
    fetch(url, options) {
      const request = { url, options }
      requests.push(request)
      return fetchImplementation(request, requests.length)
    }
  })
  vm.runInContext(coreSource
    .replace('__TEMPLATE_os_name__', '"linux"')
    .replace('__TEMPLATE_protocol_scheme__', '"http"')
    .replace('__TEMPLATE_cef__', 'true'), context)
  vm.runInContext(protocolSource
    .replace('__TEMPLATE_invoke_key__', '"synthetic-invoke-key"')
    .replace('__RAW_process_ipc_message_fn__', processMessageSource)
    .replace('__TEMPLATE_os_name__', '"linux"')
    .replace('__TEMPLATE_fetch_channel_data_command__', '"plugin:__TAURI_CHANNEL__|fetch"'), context)
  // The regular, non-isolation IPC frontend delegates to this same transport.
  window.__TAURI_INTERNALS__.ipc = window.__TAURI_INTERNALS__.postMessage
  const internals = window.__TAURI_INTERNALS__
  return { window, internals, requests, nativeMessages, invoke: internals.invoke }
}

function response(value, { error = false, type = 'application/json' } = {}) {
  return Promise.resolve({
    headers: new Headers({ 'Tauri-Response': error ? 'error' : 'ok', 'content-type': type }),
    json: async () => value,
    text: async () => value,
    arrayBuffer: async () => value
  })
}

for (const [name, ipcValue] of [['missing', undefined], ['null', null], ['empty', {}], ['non-callable', { postMessage: true }]]) {
  test(`CEF ${name} postMessage rejects one failed invoke and recovers without reload`, async () => {
    const app = runtime((_, count) => count === 1
      ? Promise.reject(new TypeError('Synthetic interrupted request'))
      : response({ ready: true }), { ipcValue })
    const originalInternals = app.window.__TAURI_INTERNALS__
    await assert.rejects(app.invoke('set_layout', { revision: 73 }), transportError)
    assert.equal(app.requests.length, 1, 'a command with uncertain delivery must not be retried')
    assert.equal(app.internals.callbacks.size, 0, 'both callbacks must be released on failure')
    assert.deepEqual(await app.invoke('profiles_list'), { ready: true })
    assert.equal(app.requests.length, 2)
    assert.equal(app.window.__TAURI_INTERNALS__, originalInternals, 'recovery must not reload the shell')
    assert.equal(app.internals.callbacks.size, 0)
  })
}

test('a failed response body is not replayed after the native command may have run', async () => {
  let nativeExecutions = 0
  const app = runtime((_, count) => {
    nativeExecutions++
    if (count !== 1) return response('ready', { type: 'text/plain' })
    return Promise.resolve({
      headers: new Headers({ 'Tauri-Response': 'ok', 'content-type': 'application/json' }),
      json: () => Promise.reject(new SyntaxError('Synthetic truncated response'))
    })
  })
  await assert.rejects(app.invoke('profile_open_source'), transportError)
  assert.equal(nativeExecutions, 1)
  assert.equal(app.internals.callbacks.size, 0)
  assert.equal(await app.invoke('profiles_list'), 'ready')
  assert.equal(nativeExecutions, 2)
})

test('parallel successful invokes survive a neighboring transport failure', async () => {
  let failFirst
  const app = runtime((_, count) => count === 1
    ? new Promise((_, reject) => { failFirst = reject })
    : response(count))
  const failing = assert.rejects(app.invoke('set_layout'), transportError)
  assert.equal(await app.invoke('profiles_list'), 2)
  failFirst(new TypeError('Synthetic interrupted request'))
  await failing
  assert.equal(await app.invoke('extensions_list'), 3)
  assert.equal(app.requests.length, 3)
  assert.equal(app.internals.callbacks.size, 0)
})

test('repeated CEF transport failures settle individually and a later call succeeds', async () => {
  const app = runtime((_, count) => count < 4
    ? Promise.reject(new TypeError('Synthetic unavailable transport'))
    : response(true))
  for (let attempt = 0; attempt < 3; attempt++) {
    await assert.rejects(app.invoke('profiles_list'), transportError)
    assert.equal(app.internals.callbacks.size, 0)
  }
  assert.equal(await app.invoke('extensions_list'), true)
  assert.equal(app.requests.length, 4)
})

test('native command errors remain errors and do not disable custom protocol IPC', async () => {
  const app = runtime((_, count) => count === 1
    ? response('Synthetic access denied', { error: true })
    : response(true))
  await assert.rejects(app.invoke('chrome_import'), (error) => error === 'Synthetic access denied')
  assert.equal(await app.invoke('profiles_list'), true)
  assert.equal(app.requests.length, 2)
  assert.equal(app.internals.callbacks.size, 0)
})

test('runtimes with a native postMessage bridge retain their existing fallback', async () => {
  const app = runtime(() => Promise.reject(new TypeError('Synthetic unsupported protocol')), {
    nativePostMessage(message, internals) {
      internals.runCallback(message.callback, 'native-result')
    }
  })
  assert.equal(await app.invoke('first_command', { count: 1 }), 'native-result')
  assert.equal(await app.invoke('second_command'), 'native-result')
  assert.equal(app.requests.length, 1)
  assert.equal(app.nativeMessages.length, 2)
  assert.equal(app.nativeMessages[0].__TAURI_INVOKE_KEY__, 'synthetic-invoke-key')
  assert.equal(app.nativeMessages[0].options.customProtocolIpcBlocked, true)
  assert.equal(app.internals.callbacks.size, 0)
})

test('HTTP invokes preserve native authorization and callback headers', async () => {
  const app = runtime(() => response(true))
  assert.equal(await app.invoke('profiles_list', { synthetic: true }), true)
  const { url, options } = app.requests[0]
  assert.equal(url, 'http://ipc.localhost/profiles_list')
  assert.equal(options.method, 'POST')
  assert.equal(options.headers.get('Tauri-Invoke-Key'), 'synthetic-invoke-key')
  assert.match(options.headers.get('Tauri-Callback'), /^\d+$/)
  assert.match(options.headers.get('Tauri-Error'), /^\d+$/)
  assert.equal(options.headers.get('Content-Type'), 'application/json')
  assert.deepEqual(JSON.parse(options.body), { synthetic: true })
})
