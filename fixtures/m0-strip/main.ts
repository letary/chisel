const greeting: string = "hi"
interface P { x: number }
function area(p: P): number { return p.x * p.x }
const sq = (n: number): number => n * n
console.log(area({ x: 3 } as P), greeting, sq(4))
