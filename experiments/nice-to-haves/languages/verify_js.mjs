import { pathToFileURL } from 'node:url';

const {greet} = await import(pathToFileURL(process.argv[2]).href);
for (const name of ['', 'é é 🌍', 'line\nquote"', 'zero\0byte']) {
  const result = greet(name);
  if (result !== `Hello, ${name}!`) throw new Error('component greeting mismatch');
}
console.log('4 JavaScript component string round trips passed');
