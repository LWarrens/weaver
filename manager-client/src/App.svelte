<script lang="ts">
  import { onMount } from 'svelte';
  import Home from './pages/Home.svelte';
  import RepoWorkspace from './pages/RepoWorkspace.svelte';

  type Route =
    | { page: 'home' }
    | { page: 'repo'; path: string };

  let route: Route = { page: 'home' };

  function parseHash(hash: string): Route {
    const h = hash.replace(/^#/, '');
    const m = h.match(/^\/repo\/(.+)$/);
    if (m) return { page: 'repo', path: decodeURIComponent(m[1]!) };
    return { page: 'home' };
  }

  function navigate(path: string) {
    window.location.hash = path;
  }

  onMount(() => {
    route = parseHash(window.location.hash);
    window.addEventListener('hashchange', () => {
      route = parseHash(window.location.hash);
    });
  });
</script>

{#if route.page === 'home'}
  <Home onNavigate={navigate} />
{:else if route.page === 'repo'}
  <RepoWorkspace repoPath={route.path} onNavigate={navigate} />
{/if}
