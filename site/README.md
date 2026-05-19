# Landing page

Static site for `https://krazyjakee.github.io/Synbad/`, deployed by
[`.github/workflows/pages.yml`](../.github/workflows/pages.yml) on every push
to `main`.

The workflow:

1. Copies `site/` to the artifact root.
2. Copies `docs/` to `/docs/` so the in-page links resolve to rendered
   GitHub-flavored markdown (Pages renders `.md` automatically with the
   default Jekyll theme).
3. Copies `assets/logo.svg` and `assets/icon.svg` to `/assets/`.

To preview locally:

```sh
cd site && python3 -m http.server 8000
# then visit http://localhost:8000
```

> **Enabling Pages.** In repo settings → Pages, set "Source" to "GitHub
> Actions". The workflow's `deploy` job needs the `github-pages` environment;
> GitHub creates it automatically the first time the workflow runs.
