const APP_SHELL_CACHE = "new-england-four-app-v67";
const DATA_CACHE = "new-england-four-data-v67";
const DATA_CACHE_TTL_MS = 24 * 60 * 60 * 1000;
const APP_SHELL_FILES = [
  "/",
  "/openfreemap_viewer.html",
  "/vendor/maplibre-gl.js",
  "/vendor/maplibre-gl.css",
];

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches.open(APP_SHELL_CACHE).then((cache) => cache.addAll(APP_SHELL_FILES))
  );
  self.skipWaiting();
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    (async () => {
      const cacheNames = await caches.keys();
      await Promise.all(
        cacheNames
          .filter((name) => ![APP_SHELL_CACHE, DATA_CACHE].includes(name))
          .map((name) => caches.delete(name))
      );
      await self.clients.claim();
    })()
  );
});

function isSameOrigin(requestUrl) {
  return requestUrl.origin === self.location.origin;
}

function isPreparedNodeRequest(url) {
  return url.pathname.includes("/prepared_nodes/") || url.pathname.includes("/local_node_store/");
}

function isBuildStatusRequest(url) {
  return url.pathname.endsWith("/build_status.json");
}

function isQueryTileStatusRequest(url) {
  return url.pathname.endsWith("/query_tiles_status.geojson");
}

function isPreviewIntersectionRequest(url) {
  return url.pathname.endsWith("/preview_intersections.geojson");
}

async function networkFirst(request, cacheName) {
  const cache = await caches.open(cacheName);
  try {
    const response = await fetch(request);
    if (response && response.ok) {
      cache.put(request, response.clone());
    }
    return response;
  } catch (error) {
    const cached = await cache.match(request);
    if (cached) {
      return cached;
    }
    throw error;
  }
}

async function cacheFirst(request, cacheName) {
  const cache = await caches.open(cacheName);
  const cached = await cache.match(request);
  if (cached) {
    return cached;
  }

  const response = await fetch(request);
  if (response && response.ok) {
    cache.put(request, response.clone());
  }
  return response;
}

async function cacheFirstWithTtl(request, cacheName, ttlMs) {
  const cache = await caches.open(cacheName);
  const cached = await cache.match(request);
  if (cached) {
    const cachedAt = Number(cached.headers.get("x-sw-cached-at") || 0);
    if (cachedAt && Date.now() - cachedAt <= ttlMs) {
      return cached;
    }
  }

  try {
    const response = await fetch(request);
    if (response && response.ok) {
      const headers = new Headers(response.headers);
      headers.set("x-sw-cached-at", String(Date.now()));
      const stamped = new Response(await response.clone().blob(), {
        status: response.status,
        statusText: response.statusText,
        headers,
      });
      cache.put(request, stamped);
    }
    return response;
  } catch (error) {
    if (cached) {
      return cached;
    }
    throw error;
  }
}

self.addEventListener("fetch", (event) => {
  const request = event.request;
  if (request.method !== "GET") {
    return;
  }

  const url = new URL(request.url);
  if (!isSameOrigin(url)) {
    return;
  }

  if (request.mode === "navigate") {
    event.respondWith(networkFirst(request, APP_SHELL_CACHE));
    return;
  }

  if (isBuildStatusRequest(url) || isQueryTileStatusRequest(url) || isPreviewIntersectionRequest(url)) {
    event.respondWith(networkFirst(request, DATA_CACHE));
    return;
  }

  if (url.pathname.startsWith("/vendor/")) {
    event.respondWith(cacheFirst(request, APP_SHELL_CACHE));
    return;
  }

  if (isPreparedNodeRequest(url)) {
    event.respondWith(cacheFirstWithTtl(request, DATA_CACHE, DATA_CACHE_TTL_MS));
    return;
  }

  if (url.pathname.endsWith("openfreemap_viewer.html")) {
    event.respondWith(networkFirst(request, APP_SHELL_CACHE));
    return;
  }

  if (
    url.pathname.endsWith("/metadata.json") ||
    url.pathname.endsWith("/tile_index.json") ||
    url.pathname.endsWith("/region_manifest.json")
  ) {
    event.respondWith(networkFirst(request, APP_SHELL_CACHE));
  }
});
