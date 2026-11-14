import { useCallback, useEffect, useState } from 'react';
import { SettingsProvider } from './settings';
import { ToastProvider } from './toast';
import { Library } from '../features/library/Library';
import { Reader } from '../features/reader/Reader';
import { SettingsDialog } from '../features/settings/SettingsDialog';
import { useShortcut } from './shortcuts';

type Route = { name: 'library' } | { name: 'reader'; documentId: string };

export function App() {
  const [route, setRoute] = useState<Route>({ name: 'library' });
  const [settingsOpen, setSettingsOpen] = useState(false);
  const openSettings = useCallback(() => setSettingsOpen(true), []);
  useShortcut('mod+,', openSettings);
  useEffect(() => {
    // Block navigation away from the app (links inside documents are handled explicitly).
    const onClick = (e: MouseEvent) => {
      const a = (e.target as HTMLElement | null)?.closest?.('a[href]');
      if (a && !a.hasAttribute('data-internal')) e.preventDefault();
    };
    document.addEventListener('click', onClick, true);
    return () => document.removeEventListener('click', onClick, true);
  }, []);
  return (
    <SettingsProvider>
      <ToastProvider>
        {route.name === 'library' ? (
          <Library onOpen={(documentId) => setRoute({ name: 'reader', documentId })} onSettings={openSettings} />
        ) : (
          <Reader key={route.documentId} documentId={route.documentId} onClose={() => setRoute({ name: 'library' })} onSettings={openSettings} />
        )}
        {settingsOpen && <SettingsDialog onClose={() => setSettingsOpen(false)} />}
      </ToastProvider>
    </SettingsProvider>
  );
}
