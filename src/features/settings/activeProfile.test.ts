import { describe, expect, it } from 'vitest';
import { describeActiveProfile, providerName } from './activeProfile';

describe('active profile display', () => {
  it('falls back to the provider kind when the label is blank', () => {
    expect(providerName('LM Studio', 'openai_compatible')).toBe('LM Studio');
    expect(providerName('  ', 'openai_compatible')).toBe('OpenAI 호환');
    expect(providerName(null, 'codex')).toBe('Codex');
    expect(providerName('', 'chatgpt')).toBe('ChatGPT 요금제');
  });
  it('says provider, model and request style in one line', () => {
    expect(describeActiveProfile({ profileId: 'p', providerId: 'v', providerLabel: '', providerKind: 'openai_compatible', providerEnabled: true, modelId: 'hy-mt2-7b', promptMode: 'simple', sourceLanguage: 'en', targetLanguage: 'ko' })).toBe('OpenAI 호환 · hy-mt2-7b · 단순');
  });
});
