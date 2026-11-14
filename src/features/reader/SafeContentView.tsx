// Renders the allow-listed SafeContent AST. Never injects HTML.
import type { ReactNode } from 'react';
import type { SafeNode } from '../../ipc/generated/SafeNode';

export interface ContentResolvers {
  resourceUrl: (resourceId: string) => string | null;
  openExternal: (url: string) => void;
  openInternal: (href: string) => void;
}

function isHttp(u: string) {
  return /^https?:\/\//i.test(u);
}

function renderNodes(nodes: SafeNode[], r: ContentResolvers, keyPrefix: string): ReactNode[] {
  return nodes.map((n, i) => {
    const k = `${keyPrefix}${i}`;
    switch (n.t) {
      case 'text': return n.v;
      case 'emphasis': return <em key={k}>{renderNodes(n.c, r, k + '.')}</em>;
      case 'strong': return <strong key={k}>{renderNodes(n.c, r, k + '.')}</strong>;
      case 'color': return <span key={k} className="ink" style={/^#[0-9a-f]{6}$/.test(n.v) ? { color: n.v } : undefined}>{renderNodes(n.c, r, k + '.')}</span>;
      case 'inline_code': return <code key={k}>{n.v}</code>;
      case 'break': return <br key={k} />;
      case 'link': {
        const href = n.href;
        const children = renderNodes(n.c, r, k + '.');
        if (isHttp(href)) {
          return <a key={k} href={href} data-internal onClick={(e) => { e.preventDefault(); r.openExternal(href); }} title={href}>{children}</a>;
        }
        return <a key={k} href="#" data-internal onClick={(e) => { e.preventDefault(); r.openInternal(href); }}>{children}</a>;
      }
      case 'image_ref': {
        const url = r.resourceUrl(n.resourceId);
        return url ? <img key={k} src={url} alt={n.alt} loading="lazy" className="inline-img" /> : <span key={k} className="faint">[{n.alt || '이미지'}]</span>;
      }
      case 'math_ref': {
        const url = r.resourceUrl(n.resourceId);
        return url ? <img key={k} src={url} alt={n.alt} className="inline-math" /> : <span key={k} className="math-fallback">{n.alt}</span>;
      }
      case 'table_cell': {
        const Tag = n.header ? 'th' : 'td';
        return <Tag key={k}>{renderNodes(n.c, r, k + '.')}</Tag>;
      }
      default: return null;
    }
  });
}

export function SafeContentView({ nodes, resolvers }: { nodes: SafeNode[]; resolvers: ContentResolvers }) {
  // table_cell runs are grouped into a simple table for preserved tables.
  if (nodes.length && nodes.every((n) => n.t === 'table_cell' || n.t === 'break')) {
    const rows: SafeNode[][] = [[]];
    for (const n of nodes) {
      if (n.t === 'break') rows.push([]);
      else rows[rows.length - 1]!.push(n);
    }
    return (
      <table className="safe-table"><tbody>
        {rows.filter((r) => r.length).map((row, i) => <tr key={i}>{renderNodes(row, resolvers, `r${i}.`)}</tr>)}
      </tbody></table>
    );
  }
  return <>{renderNodes(nodes, resolvers, '')}</>;
}
