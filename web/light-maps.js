/**
 * light-maps.js — minimal MapLibre GL integration helper for a light-maps server.
 *
 * Usage:
 *   const lm = new LightMaps({ server: "http://localhost:3000", apiKey: "…optional…" });
 *   const map = lm.createMap("map", { tileset: "world", center: [0, 20], zoom: 2 });
 *
 * Or, if you already have a MapLibre map, just add a source/layer:
 *   await lm.addTileset(map, "world");
 */

export class LightMaps {
  /**
   * @param {object} opts
   * @param {string} opts.server   - Base URL of the light-maps server (no trailing slash).
   * @param {string} [opts.apiKey] - Optional Bearer token.
   */
  constructor({ server, apiKey } = {}) {
    if (!server) throw new Error("LightMaps: server URL is required");
    this.server = server.replace(/\/$/, "");
    this.apiKey = apiKey ?? null;
  }

  // ── fetch helpers ──────────────────────────────────────────────────────────

  _headers() {
    const h = {};
    if (this.apiKey) h["Authorization"] = `Bearer ${this.apiKey}`;
    return h;
  }

  async fetchTileJSON(tileset = null) {
    const url = tileset
      ? `${this.server}/tilesets/${tileset}/tile.json`
      : `${this.server}/tile.json`;
    const res = await fetch(url, { headers: this._headers() });
    if (!res.ok) throw new Error(`light-maps: TileJSON fetch failed (${res.status})`);
    return res.json();
  }

  async listTilesets() {
    const res = await fetch(`${this.server}/tilesets`, { headers: this._headers() });
    if (!res.ok) throw new Error(`light-maps: /tilesets fetch failed (${res.status})`);
    const { tilesets } = await res.json();
    return tilesets;
  }

  // ── MapLibre integration ───────────────────────────────────────────────────

  /**
   * Add a tileset to an existing MapLibre map instance.
   *
   * Fetches TileJSON, creates a vector source, and adds a debug fill-layer
   * for each vector layer declared in the TileJSON.  Replace or extend the
   * generated layers with your own styling.
   *
   * @param {maplibregl.Map} map
   * @param {string} tileset - Tileset key (file stem of the .pmtiles file).
   * @param {object} [opts]
   * @param {boolean} [opts.addDefaultLayers=true] - Whether to add basic debug layers.
   * @returns {Promise<object>} The resolved TileJSON object.
   */
  async addTileset(map, tileset, { addDefaultLayers = true } = {}) {
    const tj = await this.fetchTileJSON(tileset);

    // Build the tiles URL with auth if needed.  MapLibre handles the actual
    // tile fetching via XHR/fetch so we inject the auth header via a
    // transformRequest hook on the map, or fall back to query-param tokens.
    const tilesUrls = this.apiKey
      ? tj.tiles.map(u => `${u}?token=${encodeURIComponent(this.apiKey)}`)
      : tj.tiles;

    map.addSource(tileset, {
      type: "vector",
      tiles: tilesUrls,
      minzoom: tj.minzoom ?? 0,
      maxzoom: tj.maxzoom ?? 14,
      bounds: tj.bounds ?? [-180, -85, 180, 85],
      attribution: tj.attribution ?? "",
    });

    if (addDefaultLayers) {
      const layers = Array.isArray(tj.vector_layers) ? tj.vector_layers : [];
      for (const layer of layers) {
        this._addDebugLayer(map, tileset, layer.id ?? layer);
      }
    }

    return tj;
  }

  _addDebugLayer(map, source, layerId) {
    map.addLayer({
      id: `lm-${source}-${layerId}`,
      type: "fill",
      source,
      "source-layer": layerId,
      paint: {
        "fill-color": _hashColor(layerId),
        "fill-opacity": 0.4,
        "fill-outline-color": _hashColor(layerId),
      },
    });
  }

  /**
   * Create a new MapLibre map pre-configured with a light-maps tileset.
   *
   * Requires maplibregl to be available on `window.maplibregl` or passed as `opts.maplibregl`.
   *
   * @param {string|HTMLElement} container - DOM id or element.
   * @param {object} opts
   * @param {string} opts.tileset
   * @param {[number,number]} [opts.center=[0,0]]
   * @param {number} [opts.zoom=2]
   * @param {object} [opts.maplibregl] - Pass the maplibregl module if not on window.
   * @returns {Promise<maplibregl.Map>}
   */
  async createMap(container, { tileset, center = [0, 0], zoom = 2, maplibregl: mgl } = {}) {
    const lib = mgl ?? window?.maplibregl;
    if (!lib) throw new Error("light-maps: maplibregl not found — pass it as opts.maplibregl");

    const tj = await this.fetchTileJSON(tileset);

    const map = new lib.Map({
      container,
      style: { version: 8, sources: {}, layers: [] },
      center: tj.center ? [tj.center[0], tj.center[1]] : center,
      zoom: tj.center?.[2] ?? zoom,
      transformRequest: this.apiKey
        ? (url) => {
            if (url.startsWith(this.server)) {
              return { url, headers: this._headers() };
            }
          }
        : undefined,
    });

    await new Promise((resolve, reject) => {
      map.once("load", resolve);
      map.once("error", reject);
    });

    await this.addTileset(map, tileset, { addDefaultLayers: true });
    return map;
  }
}

// ── utilities ──────────────────────────────────────────────────────────────

function _hashColor(str) {
  let h = 5381;
  for (let i = 0; i < str.length; i++) h = ((h << 5) + h) ^ str.charCodeAt(i);
  const r = (h & 0xff0000) >> 16;
  const g = (h & 0x00ff00) >> 8;
  const b = h & 0x0000ff;
  return `rgb(${r},${g},${b})`;
}
