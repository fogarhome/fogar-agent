import FogarWire
import SwiftUI

/// Estado: does this Mac reach Fogar right now? The headline answers; the lienzo shows the real
/// path (this app → agent → gateway) and which stretch fails; below, only what the agent reported.
struct EstadoView: View {
    @Environment(AgentLink.self) private var link
    @Environment(Navegacion.self) private var nav

    var body: some View {
        let state = link.state
        let t = Textos.titular(state)
        ScrollView {
            VStack(alignment: .leading, spacing: 40) {
                Cabecera(titular: t, state: state, since: link.since)
                if state.status?.boot_mode == "safe" { ModoSeguro() }
                Lienzo(state: state)
                if let s = state.status { Datos(status: s) }
            }
            .padding(.horizontal, 40)
            .padding(.top, 28)
            .padding(.bottom, 56)
            .frame(maxWidth: 1240, alignment: .leading)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .scrollIndicators(.automatic)
        .navigationTitle("Estado")
    }
}

private struct Cabecera: View {
    let titular: Textos.Titular
    let state: AgentState
    let since: Date
    @Environment(AgentLink.self) private var link
    @Environment(Navegacion.self) private var nav

    var body: some View {
        HStack(alignment: .top, spacing: 16) {
            VStack(alignment: .leading, spacing: 8) {
                Text(titular.titulo).titularFogar()
                    .fixedSize(horizontal: false, vertical: true)
                Text(titular.sub).font(Tipo.sub).foregroundStyle(Color.hueso)
                    .frame(maxWidth: 620, alignment: .leading)
                    .fixedSize(horizontal: false, vertical: true)
                TimelineView(.everyMinute) { tl in
                    Text(frescura(tl.date)).meta()
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            accion
        }
    }

    @ViewBuilder private var accion: some View {
        switch state {
        case .live(let s) where s.phase == .notPaired:
            Button("Emparejar este Mac") { nav.seccion = .emparejar }
                .buttonStyle(Pildora(variante: .principal))
        case .live(let s) where s.link == "parked_repair":
            Button("Ir a Ajustes") { nav.seccion = .ajustes }
                .buttonStyle(Pildora(variante: .principal))
        case .notRunning, .refused:
            Button("Volver a llamar") { link.retryNow() }
                .buttonStyle(Pildora())
        default:
            EmptyView()
        }
    }

    private func frescura(_ ahora: Date) -> String {
        switch state {
        case .live: "En vivo desde el agente · \(Textos.desde(since, ahora: ahora))"
        case .looking: "Llamando al agente…"
        case .notRunning: "Sin respuesta del agente \(Textos.desde(since, ahora: ahora)) · la app vuelve a llamar sola"
        case .versionMismatch, .refused: "El agente contesta, pero no se entienden"
        }
    }
}

private struct ModoSeguro: View {
    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Estado(tono: .warn, texto: "El agente arrancó en modo seguro").font(Tipo.texto(15, peso: 600))
            Text("Se ha caído varias veces seguidas, así que espera un rato antes de cada arranque. Si sigue pasando, hay un fallo que corregir en el agente.")
                .font(Tipo.cuerpo).suave()
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(18)
        .frame(maxWidth: 620, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: Medida.radioBloque).fill(Color.piedra))
        .accessibilityElement(children: .combine)
    }
}

// MARK: El camino — the dashboard's «Quién habla con quién», for this Mac

private enum Extremo: Hashable { case app, hub, gateway }

private struct ExtremoKey: PreferenceKey {
    static let defaultValue: [Extremo: Anchor<CGRect>] = [:]
    static func reduce(value: inout [Extremo: Anchor<CGRect>], nextValue: () -> [Extremo: Anchor<CGRect>]) {
        value.merge(nextValue()) { $1 }
    }
}

private struct Lienzo: View {
    let state: AgentState
    @Environment(Navegacion.self) private var nav

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Del Mac a tu casa").font(Tipo.h2).foregroundStyle(Color.hueso)
                .accessibilityAddTraits(.isHeader)
            HStack(alignment: .center, spacing: 24) {
                nodoApp
                    .frame(maxWidth: 300)
                    .anchorPreference(key: ExtremoKey.self, value: .bounds) { [.app: $0] }
                    .frame(maxWidth: .infinity, alignment: .leading)
                hub
                    .frame(width: 168)
                    .anchorPreference(key: ExtremoKey.self, value: .bounds) { [.hub: $0] }
                nodoGateway
                    .frame(maxWidth: 300)
                    .anchorPreference(key: ExtremoKey.self, value: .bounds) { [.gateway: $0] }
                    .frame(maxWidth: .infinity, alignment: .trailing)
            }
            .padding(.vertical, 32)
            .frame(minHeight: 260)
            // Threads under the nodes, the living background under the threads.
            .backgroundPreferenceValue(ExtremoKey.self) { anchors in
                GeometryReader { geo in Hilos(anchors: anchors, geo: geo, agente: agenteVivo, tramo: tramo) }
            }
            .background {
                FondoVivo(calor: encendido ? 0.5 : 0.3)
                    .padding(.horizontal, -40)
                    .padding(.bottom, -60)
            }
            Text("El hilo encendido es una conexión abriéndose ahora. La línea punteada es un tramo que no responde; se reintenta solo.")
                .meta()
                .frame(maxWidth: 620, alignment: .leading)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private var status: WireStatus? { state.status }
    private var agenteVivo: Bool { status != nil }
    private var encendido: Bool { status?.phase == .connecting }

    /// The agent → gateway stretch.
    private var tramo: Tramo {
        guard let s = status, s.phase != .notPaired else { return .ninguno }
        switch s.phase {
        case .ready: return .vivo
        case .connecting: return .encendido
        default: return .caido
        }
    }

    private var nodoApp: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text("Fogar Desktop").font(Tipo.texto(15, peso: 600)).foregroundStyle(Color.hueso)
            Text(status.map { "esta app · \(Textos.autenticacionCorta($0.peer_check))" } ?? "esta app")
                .meta().lineLimit(2)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .nodo()
        .accessibilityElement(children: .combine)
    }

    private var hub: some View {
        VStack(spacing: 4) {
            LineaViva(pose: status?.phase == .ready ? .rest : .line, etiqueta: "Fogar. \(Textos.agente(state))")
                .opacity(agenteVivo ? 1 : 0.45)
            Text(Textos.agente(state)).meta().lineLimit(1)
                .accessibilityHidden(true) // the line's own label says it
        }
    }

    @ViewBuilder private var nodoGateway: some View {
        if let s = status, let g = s.gateway, s.phase != .notPaired {
            VStack(alignment: .leading, spacing: 2) {
                Estado(tono: Textos.titular(state).tono, texto: g.name)
                    .font(Tipo.texto(15, peso: 600)).foregroundStyle(Color.hueso)
                Text(Textos.estadoGateway(s)).meta().lineLimit(1)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .nodo(encendido: s.phase == .connecting)
            .accessibilityElement(children: .combine)
        } else if status?.phase == .notPaired {
            Button { nav.seccion = .emparejar } label: {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Sin gateway").font(Tipo.texto(15, peso: 600)).foregroundStyle(Color.hueso)
                    Text("empareja este Mac").meta()
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .nodo(punteado: true)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel("Sin gateway. Emparejar este Mac")
        } else {
            VStack(alignment: .leading, spacing: 2) {
                Text("Tu gateway").font(Tipo.texto(15, peso: 600)).foregroundStyle(Color.huesoSuave)
                Text(state == .notRunning ? "sin noticias: el agente no contesta" : "sin noticias del agente").meta().lineLimit(1)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .nodo(punteado: true)
            .accessibilityElement(children: .combine)
        }
    }
}

private enum Tramo { case vivo, encendido, caido, ninguno }

/// The threads: hairline when alive, dashed when a stretch does not answer, ember while opening.
private struct Hilos: View {
    let anchors: [Extremo: Anchor<CGRect>]
    let geo: GeometryProxy
    let agente: Bool
    let tramo: Tramo
    @Environment(\.accessibilityReduceMotion) private var reduce

    var body: some View {
        if let a = anchors[.app], let h = anchors[.hub], let g = anchors[.gateway] {
            let ra = geo[a], rh = geo[h], rg = geo[g]
            let y = rh.midY - 10 // the word's centre, above its caption
            let p1 = CGPoint(x: ra.maxX, y: ra.midY), q1 = CGPoint(x: rh.minX - 12, y: y)
            let p2 = CGPoint(x: rh.maxX + 12, y: y), q2 = CGPoint(x: rg.minX, y: rg.midY)
            ZStack {
                hilo(p1, q1, caido: !agente)
                if tramo != .ninguno {
                    hilo(p2, q2, caido: tramo == .caido)
                    curva(p2, q2)
                        .stroke(LinearGradient(colors: [.ascua, .llama, .chispa], startPoint: .leading, endPoint: .trailing),
                                lineWidth: 2.5)
                        .opacity(tramo == .encendido ? 1 : 0)
                        .animation(reduce ? nil : .timingCurve(0.23, 1, 0.32, 1, duration: 0.3), value: tramo)
                }
                puerto(q1, on: false)
                if tramo != .ninguno { puerto(p2, on: tramo == .encendido) }
            }
            .accessibilityHidden(true)
        }
    }

    private func curva(_ a: CGPoint, _ b: CGPoint) -> Path {
        Path { p in
            let m = (a.x + b.x) / 2
            p.move(to: a)
            p.addCurve(to: b, control1: CGPoint(x: m, y: a.y), control2: CGPoint(x: m, y: b.y))
        }
    }

    private func hilo(_ a: CGPoint, _ b: CGPoint, caido: Bool) -> some View {
        curva(a, b).stroke(caido ? Color.hiloCaido : Color.hilo,
                           style: StrokeStyle(lineWidth: 1.5, dash: caido ? [5, 6] : []))
    }

    private func puerto(_ c: CGPoint, on: Bool) -> some View {
        Circle().fill(on ? Color.chispa : Color.hiloCaido).frame(width: 5, height: 5).position(c)
    }
}

// MARK: Lo que sabe el agente

private struct Datos: View {
    let status: WireStatus

    var body: some View {
        HStack(alignment: .top, spacing: 40) {
            VStack(alignment: .leading, spacing: 12) {
                Text(status.gateway.map { "Este Mac en \($0.name)" } ?? "Este Mac y el agente")
                    .font(Tipo.h2).foregroundStyle(Color.hueso).accessibilityAddTraits(.isHeader)
                VStack(spacing: 0) {
                    if let g = status.gateway {
                        Fila(clave: "Dirección", valor: g.address, seleccionable: true)
                        Fila(clave: "Nombre en el gateway", valor: g.client_name)
                        Fila(clave: "Versión del gateway", valor: g.version)
                    }
                    Fila(clave: "Cómo se autenticó esta app", valor: Textos.autenticacion(status.peer_check))
                    Fila(clave: "Arranque del agente", valor: Textos.arranque(status.boot_mode))
                }
            }
            .frame(minWidth: 380, maxWidth: .infinity, alignment: .leading)
            .accessibilityElement(children: .contain)

            VStack(alignment: .leading, spacing: 12) {
                Text("Desde que arrancó el agente").font(Tipo.h2).foregroundStyle(Color.hueso)
                    .accessibilityAddTraits(.isHeader)
                HStack(spacing: 0) {
                    Cifra(valor: status.sessions, texto: status.sessions == 1 ? "sesión con el gateway" : "sesiones con el gateway")
                    Rectangle().fill(Color.veta).frame(width: 1)
                    Cifra(valor: status.turns_completed, texto: status.turns_completed == 1 ? "turno completado" : "turnos completados")
                }
                .fixedSize(horizontal: false, vertical: true)
                .background(RoundedRectangle(cornerRadius: Medida.radioBloque).fill(Color.piedra))
            }
            .frame(width: 400)
            .accessibilityElement(children: .contain)
        }
    }
}

private struct Fila: View {
    let clave: String
    let valor: String
    var seleccionable = false

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 16) {
            Text(clave).font(Tipo.texto(14)).foregroundStyle(Color.huesoTenue)
                .frame(width: 200, alignment: .leading)
            Group {
                if seleccionable { Text(valor).textSelection(.enabled) } else { Text(valor) }
            }
            .font(Tipo.cuerpo).foregroundStyle(Color.hueso).monospacedDigit()
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.vertical, 12)
        .overlay(alignment: .top) { Rectangle().fill(Color.veta).frame(height: 1) }
        .accessibilityElement(children: .combine)
    }
}

/// Cifra: big tabular Figtree number over its label.
private struct Cifra: View {
    let valor: Int
    let texto: String
    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(valor.formatted(.number.locale(Locale(identifier: "es_ES"))))
                .font(Tipo.cifra).monospacedDigit().tracking(-0.32).foregroundStyle(Color.hueso)
                .contentTransition(.numericText(value: Double(valor)))
            Text(texto).font(Tipo.texto(14)).foregroundStyle(Color.huesoTenue)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(.horizontal, 18)
        .padding(.vertical, 16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .combine)
    }
}
