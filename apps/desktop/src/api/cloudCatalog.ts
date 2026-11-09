/**
 * The cloud model menu, by who MADE the model (as Harvey, Legora and
 * Antigravity present it), not by whose API serves it.
 *
 * Each model lists its routes: the providers that can serve it and the id it
 * goes by there, best first (the maker's own API, then aggregators). The
 * panel picks the first route whose key is connected, so one OpenRouter key
 * unlocks nearly everything, and a direct key is preferred when present.
 *
 * Ids change as vendors ship; edit this file to update the menu. Anything not
 * listed is still reachable from a provider's live model list (Keys tab).
 */

export interface CatalogModel {
    name: string;
    /** One line a lawyer can act on. */
    note: string;
    reasoning?: boolean;
    routes: { provider: string; id: string }[];
}

export interface CatalogVendor {
    vendor: string;
    models: CatalogModel[];
}

export const CATALOG: CatalogVendor[] = [
    {
        vendor: 'Anthropic',
        models: [
            { name: 'Claude Opus 4.1', note: 'Deepest analysis of long contracts and briefs', reasoning: true, routes: [{ provider: 'anthropic', id: 'claude-opus-4-1' }, { provider: 'openrouter', id: 'anthropic/claude-opus-4.1' }] },
            { name: 'Claude Sonnet 4.5', note: 'Strong drafting and review, everyday default', routes: [{ provider: 'anthropic', id: 'claude-sonnet-4-5' }, { provider: 'openrouter', id: 'anthropic/claude-sonnet-4.5' }] },
            { name: 'Claude Haiku 4.5', note: 'Fast and low-cost', routes: [{ provider: 'anthropic', id: 'claude-haiku-4-5' }, { provider: 'openrouter', id: 'anthropic/claude-haiku-4.5' }] },
        ],
    },
    {
        vendor: 'OpenAI',
        models: [
            { name: 'GPT-5', note: 'Broad reasoning across research and drafting', reasoning: true, routes: [{ provider: 'openai', id: 'gpt-5' }, { provider: 'openrouter', id: 'openai/gpt-5' }] },
            { name: 'GPT-5 mini', note: 'Quick answers at lower cost', routes: [{ provider: 'openai', id: 'gpt-5-mini' }, { provider: 'openrouter', id: 'openai/gpt-5-mini' }] },
            { name: 'GPT-4.1', note: 'Very long documents in one pass', routes: [{ provider: 'openai', id: 'gpt-4.1' }, { provider: 'openrouter', id: 'openai/gpt-4.1' }] },
            { name: 'gpt-oss 120B', note: 'Open-weights, hosted by many providers', reasoning: true, routes: [{ provider: 'groq', id: 'openai/gpt-oss-120b' }, { provider: 'nvidia', id: 'openai/gpt-oss-120b' }, { provider: 'together', id: 'openai/gpt-oss-120b' }, { provider: 'openrouter', id: 'openai/gpt-oss-120b' }] },
        ],
    },
    {
        vendor: 'Google',
        models: [
            { name: 'Gemini 2.5 Pro', note: 'Huge context: whole data rooms at once', reasoning: true, routes: [{ provider: 'gemini', id: 'gemini-2.5-pro' }, { provider: 'openrouter', id: 'google/gemini-2.5-pro' }] },
            { name: 'Gemini 2.5 Flash', note: 'Fast summaries of large files', routes: [{ provider: 'gemini', id: 'gemini-2.5-flash' }, { provider: 'openrouter', id: 'google/gemini-2.5-flash' }] },
        ],
    },
    {
        vendor: 'Meta',
        models: [
            { name: 'Llama 4 Maverick', note: 'Open-weights, multilingual', routes: [{ provider: 'groq', id: 'meta-llama/llama-4-maverick-17b-128e-instruct' }, { provider: 'nvidia', id: 'meta/llama-4-maverick-17b-128e-instruct' }, { provider: 'together', id: 'meta-llama/Llama-4-Maverick-17B-128E-Instruct-FP8' }, { provider: 'openrouter', id: 'meta-llama/llama-4-maverick' }] },
            { name: 'Llama 3.3 70B', note: 'Open-weights, dependable all-rounder', routes: [{ provider: 'groq', id: 'llama-3.3-70b-versatile' }, { provider: 'nvidia', id: 'meta/llama-3.3-70b-instruct' }, { provider: 'together', id: 'meta-llama/Llama-3.3-70B-Instruct-Turbo' }, { provider: 'openrouter', id: 'meta-llama/llama-3.3-70b-instruct' }] },
        ],
    },
    {
        vendor: 'NVIDIA',
        models: [
            { name: 'Nemotron Ultra 253B', note: 'Open reasoning model tuned by NVIDIA', reasoning: true, routes: [{ provider: 'nvidia', id: 'nvidia/llama-3.1-nemotron-ultra-253b-v1' }, { provider: 'openrouter', id: 'nvidia/llama-3.1-nemotron-ultra-253b-v1' }] },
            { name: 'Nemotron Super 49B', note: 'Efficient reasoning at lower cost', reasoning: true, routes: [{ provider: 'nvidia', id: 'nvidia/llama-3.3-nemotron-super-49b-v1' }, { provider: 'openrouter', id: 'nvidia/llama-3.3-nemotron-super-49b-v1' }] },
        ],
    },
    {
        vendor: 'DeepSeek',
        models: [
            { name: 'DeepSeek R1', note: 'Shows its reasoning step by step', reasoning: true, routes: [{ provider: 'deepseek', id: 'deepseek-reasoner' }, { provider: 'together', id: 'deepseek-ai/DeepSeek-R1' }, { provider: 'nvidia', id: 'deepseek-ai/deepseek-r1' }, { provider: 'openrouter', id: 'deepseek/deepseek-r1' }] },
            { name: 'DeepSeek V3', note: 'Capable and very low-cost', routes: [{ provider: 'deepseek', id: 'deepseek-chat' }, { provider: 'together', id: 'deepseek-ai/DeepSeek-V3' }, { provider: 'openrouter', id: 'deepseek/deepseek-chat' }] },
        ],
    },
    {
        vendor: 'Moonshot',
        models: [
            { name: 'Kimi K2', note: 'Long context, strong at structured work', routes: [{ provider: 'moonshot', id: 'kimi-k2-0905-preview' }, { provider: 'groq', id: 'moonshotai/kimi-k2-instruct' }, { provider: 'together', id: 'moonshotai/Kimi-K2-Instruct' }, { provider: 'openrouter', id: 'moonshotai/kimi-k2' }] },
        ],
    },
    {
        vendor: 'Perplexity',
        models: [
            { name: 'Sonar Pro', note: 'Answers from the live web, with citations', routes: [{ provider: 'perplexity', id: 'sonar-pro' }, { provider: 'openrouter', id: 'perplexity/sonar-pro' }] },
            { name: 'Sonar Reasoning Pro', note: 'Web research with reasoning', reasoning: true, routes: [{ provider: 'perplexity', id: 'sonar-reasoning-pro' }, { provider: 'openrouter', id: 'perplexity/sonar-reasoning-pro' }] },
            { name: 'Sonar', note: 'Quick web-grounded answers', routes: [{ provider: 'perplexity', id: 'sonar' }, { provider: 'openrouter', id: 'perplexity/sonar' }] },
        ],
    },
    {
        vendor: 'Mistral',
        models: [
            { name: 'Mistral Large', note: 'European provider, strong in French and EU law', routes: [{ provider: 'mistral', id: 'mistral-large-latest' }, { provider: 'openrouter', id: 'mistralai/mistral-large' }] },
        ],
    },
    {
        vendor: 'xAI',
        models: [
            { name: 'Grok 4', note: 'Reasoning with recent knowledge', reasoning: true, routes: [{ provider: 'xai', id: 'grok-4' }, { provider: 'openrouter', id: 'x-ai/grok-4' }] },
        ],
    },
    {
        vendor: 'Alibaba Qwen',
        models: [
            { name: 'Qwen3 235B', note: 'Open-weights, strong in Chinese and English', reasoning: true, routes: [{ provider: 'openrouter', id: 'qwen/qwen3-235b-a22b' }] },
            { name: 'Qwen3 32B', note: 'Open-weights, fast', reasoning: true, routes: [{ provider: 'groq', id: 'qwen/qwen3-32b' }, { provider: 'openrouter', id: 'qwen/qwen3-32b' }] },
        ],
    },
];
