import { Marked } from 'marked';
import hljs from 'highlight.js';
import DOMPurify from 'isomorphic-dompurify';

const marked = new Marked({
	renderer: {
		code({ text, lang }) {
			const language = lang && hljs.getLanguage(lang) ? lang : undefined;
			const highlighted = language
				? hljs.highlight(text, { language }).value
				: hljs.highlightAuto(text).value;
			const langLabel = language ?? '';
			return `<pre><code class="hljs${langLabel ? ` language-${langLabel}` : ''}">${highlighted}</code></pre>`;
		},
	},
});

export function renderMarkdown(content: string): string {
	const html = marked.parse(content, { async: false });
	return DOMPurify.sanitize(html);
}
