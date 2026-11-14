// Toolbar icons (inline SVG, currentColor). Decorative: buttons carry the aria-label.
const base = { width: 16, height: 16, viewBox: '0 0 16 16', fill: 'none', stroke: 'currentColor', strokeWidth: 1.5, strokeLinecap: 'round', strokeLinejoin: 'round', 'aria-hidden': true } as const;

export const IconPause = () => (
  <svg {...base}><rect x="3.5" y="3" width="3" height="10" rx="0.8" fill="currentColor" stroke="none" /><rect x="9.5" y="3" width="3" height="10" rx="0.8" fill="currentColor" stroke="none" /></svg>
);

export const IconPlay = () => (
  <svg {...base}><path d="M4.5 3.2v9.6a.5.5 0 0 0 .77.42l7.3-4.8a.5.5 0 0 0 0-.84l-7.3-4.8a.5.5 0 0 0-.77.42Z" fill="currentColor" stroke="none" /></svg>
);

export const IconGear = () => (
  <svg {...base}>
    <circle cx="8" cy="8" r="2.2" />
    <path d="M8 1.6v1.7M8 12.7v1.7M1.6 8h1.7M12.7 8h1.7M3.5 3.5l1.2 1.2M11.3 11.3l1.2 1.2M12.5 3.5l-1.2 1.2M4.7 11.3l-1.2 1.2" />
  </svg>
);

export const IconSidebar = () => (
  <svg {...base}><rect x="1.8" y="2.8" width="12.4" height="10.4" rx="1.8" /><path d="M6 2.8v10.4" /></svg>
);

export const IconLibrary = () => (
  <svg {...base}><path d="M2.6 2.8h2.6v10.4H2.6zM6.2 2.8h2.6v10.4H6.2z" /><path d="m10.2 4 2.4-.7 2.2 9.3-2.4.7z" /></svg>
);

export const IconExport = () => (
  <svg {...base}><path d="M8 10V2.4M5.2 5 8 2.2 10.8 5" /><path d="M3 9.4v3a1.2 1.2 0 0 0 1.2 1.2h7.6A1.2 1.2 0 0 0 13 12.4v-3" /></svg>
);

export const IconFullscreen = () => (
  <svg {...base}><path d="M2.6 6V2.6H6M10 2.6h3.4V6M13.4 10v3.4H10M6 13.4H2.6V10" /></svg>
);

export const IconFullscreenExit = () => (
  <svg {...base}><path d="M6 2.6V6H2.6M13.4 6H10V2.6M10 13.4V10h3.4M2.6 10H6v3.4" /></svg>
);
