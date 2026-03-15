# Seismic Pattern Classifier — 2D Earth Map Dashboard

A geographically accurate 2D earth map visualization for the wasmCloud
pattern-classifier system. Displays seismic events in real-time on an
equirectangular projection world map with tectonic plate boundaries.

## Features

- **Geographically accurate 2D earth map** using equirectangular projection
  with simplified but accurate coastline data for all major landmasses
- **Tectonic plate boundaries** — Mid-Atlantic Ridge, Pacific Ring of Fire,
  Alpine-Himalayan Belt, East African Rift
- **Real-time event display** — connects to NATS via WebSocket to show
  classified seismic events as they arrive
- **Magnitude-based color coding** — events colored by magnitude
  (red ≥ 6.0, orange 4.0–5.9, yellow 2.5–3.9, green < 2.5)
- **Interactive tooltips** — hover over events to see magnitude, depth,
  location, classification, and confidence
- **Demo mode** — simulated seismic events along real plate boundaries
  when no NATS connection is available
- **Dashboard stats** — total events, anomaly count, latest classification,
  and scrolling event log

## Quick Start

Open `index.html` in any modern browser:

```bash
# From the repository root:
open dashboard/index.html
# or
xdg-open dashboard/index.html
```

The dashboard starts in **demo mode** by default, showing simulated seismic
events along major plate boundaries and hotspots.

## Live Mode (NATS WebSocket)

To connect to a live NATS server with WebSocket support:

```
dashboard/index.html?nats=ws://localhost:9222
```

The dashboard subscribes to `pattern.classified.>` and expects JSON messages
with the standard classifier output format:

```json
{
  "subject": "pattern.monitor.seismic",
  "classification": "ANOMALY",
  "confidence": 0.91,
  "lat": 35.7,
  "lon": 139.7,
  "magnitude": 6.2,
  "depth_km": 45,
  "location": "Tokyo, Japan",
  "solutions": [...],
  "anomalous_fields": ["magnitude"]
}
```

### Query Parameters

| Parameter  | Default                      | Description                        |
|------------|------------------------------|------------------------------------|
| `nats`     | *(none — demo mode)*         | NATS WebSocket URL                 |
| `subject`  | `pattern.classified.>`       | NATS subject filter to subscribe   |

## Map Projection

The map uses an **equirectangular projection** (also called plate carrée),
which maps longitude directly to the x-axis and latitude to the y-axis:

```
x = (longitude + 180) / 360 × width
y = (90 - latitude)   / 180 × height
```

This projection preserves geographic relationships and is well-suited for
displaying seismic data where latitude/longitude accuracy matters more than
area preservation.

## Landmass Data

Continent outlines are simplified from Natural Earth 110m coastline data,
including:

- North America (mainland, Alaska, Mexico/Central America)
- South America
- Europe (Iberia through Scandinavia)
- Africa (including Horn of Africa)
- Asia (Russia/Siberia, Middle East, India, SE Asia, China coast)
- Australia, New Zealand
- Major islands (Greenland, Japan, Borneo, Sumatra, Java, New Guinea,
  Great Britain, Ireland, Iceland, Sri Lanka, Philippines, Taiwan, Cuba)
- Antarctica
- Arabian Peninsula, Kamchatka Peninsula

## No Dependencies

The dashboard is a single self-contained HTML file with no external
dependencies — no build step, no npm packages, no CDN resources. It works
offline and can be served from any static file host.
