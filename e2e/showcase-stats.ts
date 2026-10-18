const STATS_BASES = {
  pika: 'https://proxy.kyizl.is-a.dev/api',
  jartex: 'https://stats.jartexnetwork.com/api',
} as const;

const MAX_ATTEMPTS = 3;
const RETRY_DELAY_MS = 1_500;

export type ShowcaseNetwork = keyof typeof STATS_BASES;

export interface ShowcaseApiResult {
  profile: Record<string, unknown> | null;
  stats: unknown;
  notFound: boolean;
  rateLimit: boolean;
  statsDisabled: boolean;
}

function delay(milliseconds: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

function isRetryable(response: Response): boolean {
  return response.status === 429 || response.status >= 500;
}

async function fetchWithRetry(url: string): Promise<Response> {
  let lastError: unknown;
  for (let attempt = 1; attempt <= MAX_ATTEMPTS; attempt += 1) {
    try {
      const response = await fetch(url);
      if (!isRetryable(response) || attempt === MAX_ATTEMPTS) return response;
    } catch (error) {
      lastError = error;
      if (attempt === MAX_ATTEMPTS) throw error;
    }
    await delay(RETRY_DELAY_MS * attempt);
  }
  throw lastError;
}

function assertServed(response: Response, url: string): void {
  if (response.status >= 500) {
    throw new Error(`[showcase] ${url} responded with ${response.status}`);
  }
}

function emptyResult(overrides: Partial<ShowcaseApiResult>): ShowcaseApiResult {
  return {
    profile: null,
    stats: null,
    notFound: false,
    rateLimit: false,
    statsDisabled: false,
    ...overrides,
  };
}

export async function fetchShowcasePlayer(
  network: ShowcaseNetwork,
  username: string,
  interval: string,
  mode: string,
): Promise<ShowcaseApiResult> {
  const base = STATS_BASES[network];
  const profileUrl = `${base}/profile/${encodeURIComponent(username)}`;
  const profileResponse = await fetchWithRetry(profileUrl);
  assertServed(profileResponse, profileUrl);
  if (profileResponse.status === 404 || profileResponse.status === 400) {
    return emptyResult({ notFound: true });
  }
  if (profileResponse.status === 429) {
    return emptyResult({ rateLimit: true });
  }

  const profile = profileResponse.ok
    ? ((await profileResponse.json()) as Record<string, unknown>)
    : null;
  const canonicalUsername =
    typeof profile?.username === 'string' ? profile.username : username;

  const statsUrl = `${base}/profile/${encodeURIComponent(canonicalUsername)}/leaderboard?type=bedwars&interval=${interval}&mode=${mode}`;
  const statsResponse = await fetchWithRetry(statsUrl);
  assertServed(statsResponse, statsUrl);

  if (statsResponse.status === 204) {
    return emptyResult({ profile, statsDisabled: true });
  }
  const stats = statsResponse.ok ? ((await statsResponse.json()) as unknown) : null;
  return emptyResult({ profile, stats });
}
