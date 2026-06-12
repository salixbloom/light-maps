# MapLibre integration

light-maps serves standard TileJSON 3.0 and MVT, so it works with any vector tile client. These examples use [MapLibre GL JS](https://maplibre.org/).

## Using the `light-maps.js` helper

The bundled [`web/light-maps.js`](../web/light-maps.js) ES module handles TileJSON fetching, source/layer wiring, and optional bearer auth.

### One-liner map

```html
<script src="https://unpkg.com/maplibre-gl@4/dist/maplibre-gl.js"></script>
<link rel="stylesheet" href="https://unpkg.com/maplibre-gl@4/dist/maplibre-gl.css" />
<div id="map" style="width:100vw;height:100vh"></div>

<script type="module">
  import { LightMaps } from '/path/to/light-maps.js';

  const lm = new LightMaps({ server: 'http://localhost:3000' });
  const map = await lm.createMap('map', { tileset: 'roads' });
</script>
```

`createMap` fetches TileJSON, creates the MapLibre map, and adds a default debug layer for every vector layer in the archive. Replace it with your own styling after the map loads.

### With API key auth

```js
const lm = new LightMaps({
  server: 'https://maps.example.com',
  apiKey: 'your-secret-token',
});
```

The helper injects `Authorization: Bearer <token>` via MapLibre's `transformRequest` hook.

### Adding a tileset to an existing map

```js
const lm = new LightMaps({ server: 'http://localhost:3000' });

map.on('load', async () => {
  await lm.addTileset(map, 'roads', { addDefaultLayers: false });

  map.addLayer({
    id: 'roads-line',
    type: 'line',
    source: 'roads',
    'source-layer': 'roads',
    paint: { 'line-color': '#888', 'line-width': 1.5 },
  });
});
```

### Listing available tilesets

```js
const names = await lm.listTilesets();
// → ['roads', 'admin', 'stops']
```

---

## Plain MapLibre (no helper)

If you prefer to wire things up yourself:

```js
const map = new maplibregl.Map({
  container: 'map',
  style: {
    version: 8,
    sources: {
      roads: {
        type: 'vector',
        url: 'http://localhost:3000/tilesets/roads/tile.json',
      },
    },
    layers: [
      {
        id: 'roads-line',
        type: 'line',
        source: 'roads',
        'source-layer': 'roads',
        paint: { 'line-color': '#555', 'line-width': 1.2 },
      },
    ],
  },
  center: [0, 20],
  zoom: 2,
});
```

---

## React snippet

```jsx
import { useEffect, useRef } from 'react';
import maplibregl from 'maplibre-gl';
import 'maplibre-gl/dist/maplibre-gl.css';

export function Map({ server, tileset }) {
  const container = useRef(null);

  useEffect(() => {
    const map = new maplibregl.Map({
      container: container.current,
      style: {
        version: 8,
        sources: {
          [tileset]: { type: 'vector', url: `${server}/tilesets/${tileset}/tile.json` },
        },
        layers: [],
      },
      center: [0, 20],
      zoom: 2,
    });

    map.on('load', () => {
      // add your layers here
    });

    return () => map.remove();
  }, [server, tileset]);

  return <div ref={container} style={{ width: '100%', height: '100%' }} />;
}
```

---

## Multi-tileset style

```js
sources: {
  roads:  { type: 'vector', url: 'http://localhost:3000/tilesets/roads/tile.json' },
  admin:  { type: 'vector', url: 'http://localhost:3000/tilesets/admin/tile.json' },
  stops:  { type: 'vector', url: 'http://localhost:3000/tilesets/stops/tile.json' },
},
```

Each archive is a separate source; all are served by the same `lm-serve` process.
