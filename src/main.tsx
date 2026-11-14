import { createRoot } from 'react-dom/client';
import { App } from './app/App';
import './app/theme.css';

createRoot(document.getElementById('root')!).render(<App />);

if (import.meta.env.DEV) {
  void import('./app/selftest').then((m) => {
    m.installErrorForwarding();
    setTimeout(() => void m.runSelftest(), 1500);
  });
}
