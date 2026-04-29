declare type PlayerId = string

declare class Player
    id: PlayerId
    name: string
    score: number
    function addScore(self, points: number): ()
end

declare class ScoreBoard
    function top(self, count: number): { Player }
    function record(self, player: Player): ()
end

declare Game: {
    createPlayer: (id: PlayerId, name: string) -> Player,
    scoreboard: () -> ScoreBoard,
}
