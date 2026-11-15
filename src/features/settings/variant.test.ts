import { describe, expect, it } from 'vitest';
import { variantOf } from './SettingsDialog';

describe('variantOf', () => {
  it('reads account-backed kinds from the capabilities variant', () => {
    expect(variantOf({ kind: 'openai_compatible', capabilities: { variant: 'chatgpt' } })).toBe('chatgpt');
    expect(variantOf({ kind: 'openai_compatible', capabilities: { variant: 'codex' } })).toBe('codex');
    expect(variantOf({ kind: 'openai_compatible', capabilities: {} })).toBe('openai_compatible');
    expect(variantOf({ kind: 'openai_compatible', capabilities: { variant: 'something-else' } })).toBe('openai_compatible');
    expect(variantOf({ kind: 'ollama', capabilities: {} })).toBe('ollama');
  });
});
