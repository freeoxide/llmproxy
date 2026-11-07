'use strict';

// The JS layer over the napi-generated loader (binding.js): it unpacks the
// native structured results into thrown ProxyErrors and an async-iterable
// stream. No engine logic lives here.

const native = require('./binding.js');

class ProxyError extends Error {
  constructor(info) {
    super(info.message);
    this.name = 'ProxyError';
    this.kind = info.kind;
    this.status = info.status;
    this.message = info.message;
    this.retryAfterSeconds = info.retryAfterSeconds;
    this.shouldRetry = info.shouldRetry;
  }
}

function unwrap(result) {
  if (result.error) {
    throw new ProxyError(result.error);
  }
  return result.response;
}

class Stream {
  constructor(nativeStream) {
    this._native = nativeStream;
  }

  async next() {
    return this._native.next();
  }

  async cancel() {
    return this._native.cancel();
  }

  get cancelled() {
    return this._native.cancelled;
  }

  [Symbol.asyncIterator]() {
    return {
      next: async () => {
        const event = await this._native.next();
        return event === null || event === undefined
          ? { done: true, value: undefined }
          : { done: false, value: event };
      },
    };
  }
}

class Proxy {
  constructor(config) {
    this._native = new native.Proxy(config);
  }

  async execute(provider, dialect, body, options) {
    return unwrap(await this._native.execute(provider, dialect, body, options));
  }

  async executeStream(provider, dialect, body, options) {
    let stream;
    try {
      stream = await this._native.executeStream(provider, dialect, body, options);
    } catch (error) {
      // The native layer throws a napi error whose message is the JSON
      // encoded failure info (see src/lib.rs); keep it structured.
      let info;
      try {
        info = JSON.parse(error.message);
      } catch {
        throw error;
      }
      throw new ProxyError(info);
    }
    return new Stream(stream);
  }

  translateRequest(body, source, target, model) {
    const result = this._native.translateRequest(body, source, target, model);
    if (result.error) {
      throw new ProxyError(result.error);
    }
    return result.result;
  }

  translateResponse(body, source, target, model) {
    const result = this._native.translateResponse(body, source, target, model);
    if (result.error) {
      throw new ProxyError(result.error);
    }
    return result.result;
  }

  usageEvents() {
    return this._native.usageEvents();
  }
}

module.exports = { Proxy, ProxyError, Stream };
