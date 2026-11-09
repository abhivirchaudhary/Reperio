# Red Team Forensic Suite

The Red Team Forensic Suite extends Reperio with enterprise-grade forensic
capabilities found in tools like **Magnet AXIOM**, **Cellebrite UFED**,
**Oxygen Detective**, **MSAB XRY**, **EnCase**, **FTK**, and **Volatility**.

All plugins are read-only, run via Reperio's existing JSON-RPC analyzer
protocol, and are fully audited in `audit.log`.

> **These plug-ins are not part of this repository.** They are available on
> request: open an issue or contact Abhivir Chaudhary through
> [GitHub](https://github.com/abhivirchaudhary). What is open here is Reperio's
> side of it: the `reperio redteam` command, the analyzer protocol, and this
> guide for writing plug-ins of your own.

---

## Quick Start

```bash
# Run the full redteam suite against a disk image
reperio redteam image.img ./redteam-out

# Filter by category
reperio redteam image.img ./out --category credentials

# Only high-confidence findings
reperio redteam image.img ./out --confidence high

# Aggressive mode: also scan VSS snapshots
reperio redteam image.img ./out --aggressive
```

The command produces:
- `./redteam-out/redteam-summary.json` — aggregated findings from all plugins
- `./redteam-out/<plugin-name>/` — per-plugin reports, outputs, and audit logs
- `./redteam-out/<plugin-name>/evtx-timeline.csv` — event log timeline (from evtx-carver)
- `./redteam-out/<plugin-name>/locations.kml` — Google Earth KML (from geo-correlator)

---

## Plugin Catalog

### Mobile Deep Extraction (Cellebrite / XRY-class)

| Plugin | Targets | What it finds |
|--------|---------|---------------|
| `ios-deep-extract` | image | iTunes backup Manifest.db, keychain, deleted Photos.sqlite markers, Health, Screen Time |
| `android-deep-extract` | android, image | SMS/MMS (mmssms.db), calls, contacts, WhatsApp/Signal/Telegram DBs, WiFi configs |

### Communication & Cloud Apps (AXIOM / Oxygen-class)

| Plugin | Targets | What it finds |
|--------|---------|---------------|
| `comm-app-hunter` | image | Slack tokens (xoxp-/xoxb-), Discord tokens, Teams IndexedDB, Signal config.json, Telegram tdata |
| `cloud-sync-hunter` | image | Dropbox config.db/host_id, Google Drive snapshot.db, OneDrive settings.dat, iCloud ubd tokens |

### Browser Credential Extraction (AXIOM / FTK-class)

| Plugin | Targets | What it finds |
|--------|---------|---------------|
| `browser-forensics` | image | Chrome/Edge/Brave Login Data, Cookies, History; Firefox logins.json; Safari binarycookies |

### Windows Registry Deep Forensics (EnCase / FTK-class)

| Plugin | Targets | What it finds |
|--------|---------|---------------|
| `registry-deep-dive` | image | Shellbags, USBSTOR timeline with serials, SAM hive hash extraction hints, UserAssist, deleted keys |

### Windows Artifact Hunter (EnCase / AXIOM-class)

| Plugin | Targets | What it finds |
|--------|---------|---------------|
| `windows-artifact-hunter` | image | Prefetch (.pf), SRUDB.dat, Jump Lists, LNK shortcuts, Recycle Bin $I files, RDP bitmap cache |

### NTFS Deep Analysis

| Plugin | Targets | What it finds |
|--------|---------|---------------|
| `ntfs-redteam` | image | Alternate Data Streams (ADS), $LogFile restart pages, $UsnJrnl deletions, MFT timestomp flags |

### Event Log Carving

| Plugin | Targets | What it finds |
|--------|---------|---------------|
| `evtx-carver` | image | Carved EVTX chunks filtered for Event IDs 4624/4625/4648/4672/4768/4769/4776/1102/104/4103/4104/1149 |

### Memory & Hibernation Analysis (Volatility-class)

| Plugin | Targets | What it finds |
|--------|---------|---------------|
| `memory-hunter` | image | hiberfil.sys / raw dumps: processes, network connections, credential material (NTLMSSP/Kerberos), injected code |

### Lateral Movement & Persistence (Velociraptor-class)

| Plugin | Targets | What it finds |
|--------|---------|---------------|
| `lateral-movement-hunter` | image | WMI __EventFilter/__EventConsumer, scheduled task XML, service registry, RDP artifacts, network share history |

### Anti-Forensics Detection

| Plugin | Targets | What it finds |
|--------|---------|---------------|
| `anti-forensics` | image | CCleaner/BleachBit/Eraser signatures, MFT timestamp anomalies, USN journal wiping, Event ID 1102 |

### Encrypted Container Detection (Passware / Elcomsoft-class)

| Plugin | Targets | What it finds |
|--------|---------|---------------|
| `crypto-hunter` | image | BitLocker FVE, VeraCrypt headers, LUKS, TrueCrypt, FileVault wrappers, entropy analysis |

### Geolocation Correlation

| Plugin | Targets | What it finds |
|--------|---------|---------------|
| `geo-correlator` | image | EXIF GPS from JPEG/HEIC, Android cache.cell/cache.wifi, iOS locationd → KML + CSV timeline |

### Steganography & Misdirection

| Plugin | Targets | What it finds |
|--------|---------|---------------|
| `stego-misdirection` | image | Extension vs magic mismatch, ADS on NTFS, appended data after JPEG EOI, stego tool signatures |

---

## Plugin Developer Guide

Redteam plugins are ordinary Reperio analyzer plugins. In the full suite they
live in a `redteam/` folder beside the other plug-ins and speak the same
JSON-RPC protocol as any other plugin.

### Severity Levels

The engine accepts four severity levels:
- `info` — informational
- `warn` — suspicious but not confirmed
- `important` — confirmed artifact
- `critical` — credentials, encryption keys, or active compromise evidence

### Tags

Include `tags` on findings so the CLI `--category` filter works:
```python
run.finding(
    "USB Device",
    "Vendor=SanDisk Serial=12345",
    severity="important",
    tags=["usb", "hardware"],
    confidence="medium",
)
```

Common tags: `credentials`, `lateral_movement`, `persistence`, `anti_forensics`,
`location`, `comms`, `cloud`, `encryption`, `memory`, `execution`, `ntfs`.

### Confidence

Set `confidence` to help operators prioritize:
- `high` — directly observed data (registry value, database row)
- `medium` — strong heuristic match (signature, header pattern)
- `low` — weak indicator requiring manual verification

### Shared Libraries

The suite's `lib/` folder contains pure-Python helpers:
- `registry.py` — Windows registry hive parser
- `evtx.py` — EVTX chunk parser
- `sqlite_deep.py` — Deleted row / WAL recovery
- `memory_structs.py` — Windows kernel structures for memory dumps
- `kml.py` — Google Earth KML writer
- `timeline.py` — MACB correlation and CSV writer
- `entropy.py` — Shannon entropy calculator

Import them from your plugin:
```python
import sys, os
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "lib"))
from registry import RegistryHive
```

---

## Chain of Custody

Every redteam plugin run produces:
- `audit.log` — every shell command, file pull, and image read offset
- `MANIFEST.sha256.txt` — hashes of all output files
- `report.json` — structured findings, outputs, and errors

These live in `DEST/<plugin-name>/` and are admissible alongside Reperio's
standard forensic acquisition logs.
