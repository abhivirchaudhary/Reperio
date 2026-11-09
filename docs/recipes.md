# Recipes: chaining plug-ins

A recipe is one JSON file that describes a whole job. Plug-ins chain:

1. a **source** plug-in provides the bytes of a device Reperio cannot reach,
2. Reperio reads the file systems on them (and your **reader** plug-ins do),
   and deep-scans the free space,
3. **analyzer** plug-ins look at the same source and can offer recoverable
   files of their own,
4. everything is recovered together, with one report.

    reperio recipe example > job.json
    reperio recipe run job.json

```json
{
  "source": "plugin:ios-usb:00008030-0012",
  "readers": ["myfs"],
  "analyzers": ["whatsapp-media"],
  "options": { "whatsapp-media": { "deep": true } },
  "deep": true,
  "layout": "original",
  "recover": "./recovered",
  "reports": "./reports"
}
```

| key | meaning |
|---|---|
| `source` | a disk, an image file, or `plugin:NAME:DEVICE` (see `reperio devices`) |
| `readers` | reader plug-ins to use; leave out for all installed, `[]` for none |
| `analyzers` | analyzer plug-ins to run on the source after the scan; files they offer join the results |
| `options` | settings for plug-ins for this run only, not saved |
| `deep`, `existing`, `damaged`, `types` | as for `reperio scan` |
| `layout` | `original`, `by-date` or `by-type` |
| `recover` | where files go; leave out to only list what is found |
| `reports` | where analyzer reports go |
| `save` | save the scan's results to open later in the app |

The same chain from the command line, without a file:

    reperio scan plugin:ios-usb:00008030-0012 --plugin myfs --analyze whatsapp-media -o ./recovered

`scripts/recipe_check.sh` runs a recipe end to end (a source plug-in, an
analyzer that offers a file nothing else could find, and the normal scan) and
checks both recovered files byte for byte.
