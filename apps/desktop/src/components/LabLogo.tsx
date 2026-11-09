/**
 * Small company logos for model makers, as on models.dev.
 *
 * The SVGs are the models.dev provider logos (MIT-licensed project; logos are
 * the property of their companies), bundled with the app so they work offline.
 * They are drawn in `currentColor` and inlined, so each one takes the colour of
 * the text beside it: black in the light theme, white in the dark theme.
 */

import alibaba from '../assets/labs/alibaba.svg?raw'
import anthropic from '../assets/labs/anthropic.svg?raw'
import bytedance from '../assets/labs/bytedance.svg?raw'
import cohere from '../assets/labs/cohere.svg?raw'
import deepseek from '../assets/labs/deepseek.svg?raw'
import fallback from '../assets/labs/default.svg?raw'
import google from '../assets/labs/google.svg?raw'
import huggingface from '../assets/labs/huggingface.svg?raw'
import ibm from '../assets/labs/ibm.svg?raw'
import meta from '../assets/labs/meta.svg?raw'
import microsoft from '../assets/labs/microsoft.svg?raw'
import minimax from '../assets/labs/minimax.svg?raw'
import mistral from '../assets/labs/mistral.svg?raw'
import moonshot from '../assets/labs/moonshotai.svg?raw'
import nvidia from '../assets/labs/nvidia.svg?raw'
import openai from '../assets/labs/openai.svg?raw'
import stepfun from '../assets/labs/stepfun.svg?raw'
import xai from '../assets/labs/xai.svg?raw'
import zhipu from '../assets/labs/zhipuai.svg?raw'

const LOGOS: Record<string, string> = {
  Alibaba: alibaba,
  Anthropic: anthropic,
  ByteDance: bytedance,
  Cohere: cohere,
  DeepSeek: deepseek,
  Google: google,
  'Hugging Face': huggingface,
  IBM: ibm,
  Meta: meta,
  Microsoft: microsoft,
  MiniMax: minimax,
  'Mistral AI': mistral,
  'Moonshot AI': moonshot,
  NVIDIA: nvidia,
  OpenAI: openai,
  StepFun: stepfun,
  xAI: xai,
  'Zhipu AI': zhipu,
}

/** Make every logo render at the same size, whatever its own width/height say. */
function sized(svg: string): string {
  return svg.replace(/<svg\b([^>]*)>/, (_m, attrs: string) => {
    const rest = attrs.replace(/\s(width|height)="[^"]*"/g, '')
    return `<svg${rest} width="100%" height="100%" aria-hidden="true" focusable="false">`
  })
}

const SIZED = Object.fromEntries(Object.entries(LOGOS).map(([k, v]) => [k, sized(v)]))
const SIZED_FALLBACK = sized(fallback)

export function hasLabLogo(company: string): boolean {
  return company in LOGOS
}

export default function LabLogo({ company }: { company: string }) {
  return (
    <span
      className="lab-logo"
      aria-hidden="true"
      // Bundled, static SVG files from this repository — never user or network content.
      dangerouslySetInnerHTML={{ __html: SIZED[company] ?? SIZED_FALLBACK }}
    />
  )
}
