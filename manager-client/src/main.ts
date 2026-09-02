import './styles.css';
import { mount } from 'svelte';
import App from './App.svelte';

const target = document.getElementById('root');
if (!target) throw new Error('Root element #root not found');

try {
  mount(App, { target });
} catch (err) {
  target.innerHTML = `<pre style="color:#f4845f;padding:2rem;font-family:monospace">Failed to mount app:\n${err instanceof Error ? err.stack ?? err.message : String(err)}</pre>`;
  console.error('Mount failed:', err);
}
