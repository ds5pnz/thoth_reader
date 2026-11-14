import { describe, expect, it } from 'vitest';
import { defaultPromptMode, isTranslationModel } from './SettingsDialog';

describe('isTranslationModel', () => {
  it('recognises translation-only models by id', () => {
    for (const id of ['translategemma-4b-it', 'hy-mt2-7b', 'tencent/Hunyuan-MT-7B', 'madlad400-3b-mt', 'nllb-200-distilled', 'Helsinki-NLP/opus-mt-en-ko']) expect(isTranslationModel(id)).toBe(true);
  });
  it('leaves chat models on the structured request', () => {
    for (const id of ['qwen3.8-27b', 'gemma-3-12b-it', 'llama3.3:70b', 'gpt-4o-mini', 'mistral-small', '']) expect(isTranslationModel(id)).toBe(false);
  });
  it('picks the request style from the model id', () => {
    expect(defaultPromptMode('yanolja_yanoljanext-rosetta-12b-2510')).toBe('rosetta');
    expect(defaultPromptMode('hy-mt2-7b')).toBe('simple');
    expect(defaultPromptMode('qwen3.8-27b')).toBe('structured');
  });
});
