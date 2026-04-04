import { describe, it, expect } from 'vitest';
import { renderMarkdown } from './markdown';

describe('renderMarkdown', () => {
	it('renders basic markdown', () => {
		const result = renderMarkdown('**bold** and *italic*');
		expect(result).toContain('<strong>bold</strong>');
		expect(result).toContain('<em>italic</em>');
	});

	it('renders code blocks with syntax highlighting', () => {
		const result = renderMarkdown('```rust\nfn main() {}\n```');
		expect(result).toContain('<pre>');
		expect(result).toContain('<code');
		expect(result).toContain('fn');
	});

	it('strips <script> tags', () => {
		const result = renderMarkdown('<script>alert("xss")</script>');
		expect(result).not.toContain('<script');
		expect(result).not.toContain('alert(');
	});

	it('strips onerror event handlers', () => {
		const result = renderMarkdown('<img src=x onerror="alert(1)">');
		expect(result).not.toContain('onerror');
	});

	it('strips javascript: URIs in links', () => {
		const result = renderMarkdown('[click](javascript:alert(1))');
		expect(result).not.toContain('javascript:');
	});

	it('strips onload event handlers in images', () => {
		const result = renderMarkdown('<img src="valid.png" onload="alert(1)">');
		expect(result).not.toContain('onload');
	});
});
