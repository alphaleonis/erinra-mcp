import { existsSync, rmSync } from 'node:fs';
import path from 'node:path';

const E2E_DATA_DIR = path.resolve(process.cwd(), '.e2e-data');

export default function globalTeardown() {
	if (existsSync(E2E_DATA_DIR)) {
		rmSync(E2E_DATA_DIR, { recursive: true, force: true });
	}
}
