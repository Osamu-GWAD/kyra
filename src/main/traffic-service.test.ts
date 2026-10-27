import { Buffer } from 'node:buffer';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const storage = vi.hoisted(() => ({
  directory: '',
  available: true,
  backend: 'gnome_libsecret',
}));
vi.mock('electron', () => ({
  app: { getPath: () => storage.directory },
  safeStorage: {
    isEncryptionAvailable: () => storage.available,
    getSelectedStorageBackend: () => storage.backend,
    encryptString: vi.fn((value: string) => Buffer.from([...value].reverse().join(''))),
    decryptString: vi.fn((value: Buffer) => [...value.toString()].reverse().join('')),
  },
}));

beforeEach(() => {
  vi.resetModules();
  storage.directory = mkdtempSync(join(tmpdir(), 'kyra-traffic-'));
  storage.available = true;
  storage.backend = 'gnome_libsecret';
});

afterEach(() => rmSync(storage.directory, { recursive: true, force: true }));

describe('traffic settings', () => {
  it('defaults to unauthenticated middleware and isolates Jartex', async () => {
    const traffic = await import('./traffic-service');
    expect(traffic.getTrafficStatus()).toEqual({ mode: 'middleware', hasApiKey: false });
    expect(traffic.getStatsRoute('pika')).toMatchObject({
      base: 'https://proxy.kyizl.is-a.dev/api',
      headers: {},
    });
    traffic.saveTrafficApiKey('test-secret');
    expect(traffic.getStatsRoute('pika')).toMatchObject({
      base: 'https://stats.pika-network.net/api',
      headers: { 'X-Api-Key': 'test-secret' },
    });
    expect(traffic.getStatsRoute('jartex')).toMatchObject({
      base: 'https://stats.jartexnetwork.com/api',
      headers: {},
    });
    traffic.setTrafficMode('middleware');
    expect(traffic.getStatsRoute('pika').headers).toEqual({});
    expect(traffic.getTrafficStatus().hasApiKey).toBe(true);
  });

  it('encrypts, reloads, replaces, and removes the saved key', async () => {
    let traffic = await import('./traffic-service');
    const initialScope = traffic.getStatsRoute('pika').cacheScope;
    expect(traffic.saveTrafficApiKey('  first-secret  ')).toEqual({
      mode: 'direct',
      hasApiKey: true,
    });
    expect(traffic.getStatsRoute('pika').cacheScope).not.toBe(initialScope);
    expect(readFileSync(join(storage.directory, 'traffic.json'), 'utf8')).not.toContain(
      'first-secret',
    );
    vi.resetModules();
    traffic = await import('./traffic-service');
    expect(traffic.getStatsRoute('pika').headers).toEqual({
      'X-Api-Key': 'first-secret',
    });
    const savedScope = traffic.getStatsRoute('pika').cacheScope;
    traffic.saveTrafficApiKey('replacement-secret');
    expect(traffic.getStatsRoute('pika').cacheScope).not.toBe(savedScope);
    expect(traffic.getStatsRoute('pika').headers).toEqual({
      'X-Api-Key': 'replacement-secret',
    });
    expect(traffic.removeTrafficApiKey()).toEqual({
      mode: 'middleware',
      hasApiKey: false,
    });
    expect(
      JSON.parse(readFileSync(join(storage.directory, 'traffic.json'), 'utf8')),
    ).toEqual({ mode: 'middleware' });
    vi.resetModules();
    traffic = await import('./traffic-service');
    expect(traffic.getTrafficStatus()).toEqual({ mode: 'middleware', hasApiKey: false });
    expect(() => traffic.setTrafficMode('direct')).toThrow('Save an API key');
  });

  it('rejects invalid keys and insecure storage without changing the active settings', async () => {
    const traffic = await import('./traffic-service');
    for (const key of ['', '   ', 'a\r\nb', 'has spaces', '☃']) {
      expect(() => traffic.saveTrafficApiKey(key)).toThrow('valid API key');
    }
    storage.available = false;
    expect(() => traffic.saveTrafficApiKey('secret')).toThrow('Secure storage');
    storage.available = true;
    storage.backend = 'basic_text';
    expect(() => traffic.saveTrafficApiKey('secret')).toThrow('Secure storage');
    expect(traffic.getTrafficStatus()).toEqual({ mode: 'middleware', hasApiKey: false });
  });
});
