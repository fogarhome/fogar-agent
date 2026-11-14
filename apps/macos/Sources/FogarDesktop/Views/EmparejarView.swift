import FogarWire
import SwiftUI

/// Emparejar: the dashboard's Conectar screen for this Mac. On one side the brand (living
/// background, living line, one line of purpose); on the other, the only thing the person types:
/// where the gateway is (found or by hand) and the 6-digit code. Everything else the agent sets.
struct EmparejarView: View {
    @Environment(AgentLink.self) private var link

    var body: some View {
        HStack(spacing: 0) {
            Marca()
                .zIndex(-1)
                .frame(minWidth: 300, maxWidth: .infinity, maxHeight: .infinity)
                .ignoresSafeArea(edges: .top)
            ScrollView {
                Group {
                    switch link.state {
                    case .live(let s) where s.phase == .notPaired:
                        Formulario()
                    case .live(let s):
                        YaEmparejado(status: s)
                    default:
                        SinAgente(state: link.state)
                    }
                }
                .frame(maxWidth: 420, alignment: .leading)
                .padding(.horizontal, 48)
                .padding(.top, 108) // clears the title bar, then the dashboard's 56
                .padding(.bottom, 56)
                .frame(maxWidth: .infinity, minHeight: 560, alignment: .leading)
            }
            .frame(minWidth: 420, maxWidth: .infinity)
            .modifier(SinBordeDeScroll())
        }
        .ignoresSafeArea(edges: .top)
        .navigationTitle("Emparejar")
    }
}

private struct Marca: View {
    var body: some View {
        VStack(alignment: .leading, spacing: 48) {
            LineaViva(pose: .rest, etiqueta: "Fogar")
                .frame(maxWidth: 440)
                .padding(.leading, -24)
            Spacer(minLength: 0)
            Text("Fogar Desktop une este Mac a tu casa: el agente habla con tu gateway y esta app te cuenta cómo va.")
                .font(Tipo.texto(14)).suave()
                .frame(maxWidth: 360, alignment: .leading)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(.horizontal, 56)
        .padding(.top, 108)
        .padding(.bottom, 56)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background {
            // Bleeds past the panel like the dashboard's (inset -10% -20% -30% -30%); the mask fades it.
            FondoVivo(calor: 0.45, humo: 0.9)
                .padding(.top, -80).padding(.bottom, -200).padding(.leading, -200).padding(.trailing, -160)
                .ignoresSafeArea()
        }
    }
}

private struct Formulario: View {
    @Environment(AgentLink.self) private var link
    @Environment(Navegacion.self) private var nav

    @State private var candidatos: [WireCandidate] = []
    @State private var buscando = false
    @State private var direccion = ""
    @State private var codigo = ""
    @State private var errorDireccion: String?
    @State private var errorCodigo: String?
    @State private var errorGeneral: String?
    @State private var emparejando = false
    @FocusState private var focoDireccion: Bool
    @FocusState private var focoCodigo: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 24) {
            VStack(alignment: .leading, spacing: 12) {
                Text("Hola. Emparejemos este Mac.").titularFogar()
                    .fixedSize(horizontal: false, vertical: true)
                Text("Abre un código en tu gateway: en su panel, en Clientes → Emparejar, o con fogar-gw pair. Aquí solo hace falta escribirlo.")
                    .font(Tipo.cuerpo).suave()
                    .fixedSize(horizontal: false, vertical: true)
            }

            encontrados

            Campo(etiqueta: "Dirección del gateway", pista: "Sin http://; si no pones puerto, es el 8004.", error: errorDireccion, campo: {
                TextField("", text: $direccion, prompt: Text("green-house o 192.168.1.20:8004").foregroundStyle(Color.huesoTenue))
                    .autocorrectionDisabled()
                    .onChange(of: direccion) { errorDireccion = nil }
            }, enfocado: $focoDireccion)

            Campo(etiqueta: "Código de 6 cifras", pista: "Vale una sola vez y caduca a los 10 minutos.", error: errorCodigo, campo: {
                TextField("", text: $codigo, prompt: Text("482 913").foregroundStyle(Color.huesoTenue))
                    .textContentType(.oneTimeCode)
                    .autocorrectionDisabled()
                    .font(Tipo.texto(22, peso: 500)).monospacedDigit().tracking(2)
                    // Typing clears the error; the app emptying a spent code must not.
                    .onChange(of: codigo) { _, nuevo in if !nuevo.isEmpty { errorCodigo = nil; errorGeneral = nil } }
                    .onSubmit(emparejar)
            }, enfocado: $focoCodigo)

            if let errorGeneral {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    EstadoIcono(tono: .bad, tam: 12)
                    Text(errorGeneral).font(Tipo.cuerpo).foregroundStyle(Color.hueso)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .accessibilityElement(children: .combine)
            }

            // Busy, not disabled: the label keeps full contrast; `emparejar()` ignores repeats.
            Button(action: emparejar) {
                Text(emparejando ? "Emparejando…" : "Emparejar")
            }
            .buttonStyle(Pildora(variante: .principal, ocupado: emparejando, ancho: true))
            .keyboardShortcut(.defaultAction)
        }
        .task { await buscar() }
        .onDisappear { codigo = "" }
    }

    @ViewBuilder private var encontrados: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text("Gateways encontrados").font(Tipo.etiqueta).foregroundStyle(Color.hueso)
                Spacer()
                Button(buscando ? "Buscando…" : "Buscar otra vez") { Task { await buscar() } }
                    .buttonStyle(Pildora(pequena: true, ocupado: buscando))
            }
            if candidatos.isEmpty {
                Text(buscando ? "Mirando en la dirección guardada, en este Mac y en tu tailnet…"
                              : "No he encontrado ninguno. Escribe su dirección aquí abajo.")
                    .meta()
                    .fixedSize(horizontal: false, vertical: true)
            } else {
                VStack(spacing: 8) {
                    ForEach(candidatos, id: \.address) { c in
                        Candidato(c: c, elegido: direccion == sinEsquema(c.address)) {
                            direccion = sinEsquema(c.address)
                            focoCodigo = true
                        }
                    }
                }
            }
        }
    }

    private func buscar() async {
        guard !buscando else { return }
        buscando = true
        defer { buscando = false }
        do {
            candidatos = try await link.discover()
            if candidatos.count == 1, direccion.isEmpty { direccion = sinEsquema(candidatos[0].address) }
            // Keyboard first: straight to what is still missing.
            if !focoDireccion && !focoCodigo {
                if direccion.isEmpty { focoDireccion = true } else { focoCodigo = true }
            }
        } catch {
            candidatos = []
            errorGeneral = Textos.falloBusqueda(error)
        }
    }

    private func emparejar() {
        guard !emparejando else { return }
        errorDireccion = nil; errorCodigo = nil; errorGeneral = nil
        let dir = direccion.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !dir.isEmpty else {
            errorDireccion = "Elige un gateway o escribe su dirección."
            focoDireccion = true
            return
        }
        guard let digits = parsePairCode(codigo) else {
            // Checked here so a typo never spends one of the gateway's 5 attempts.
            errorCodigo = Textos.fallo(.badCode)
            focoCodigo = true
            return
        }
        emparejando = true
        Task {
            defer { emparejando = false }
            var sigueValiendo = false // the gateway did not consume it: keep it for the retry
            defer { if !sigueValiendo { codigo = "" } }
            do {
                switch try await link.pair(address: dir, code: digits) {
                case .paired:
                    nav.seccion = .estado
                case .failed(let f):
                    switch f {
                    case .badAddress, .unreachable, .notAGateway:
                        sigueValiendo = true
                        errorDireccion = Textos.fallo(f); focoDireccion = true
                    case .badCode, .invalidCode:
                        errorCodigo = Textos.fallo(f); focoCodigo = true
                    case .gatewayError, .busy:
                        sigueValiendo = true
                        errorGeneral = Textos.fallo(f)
                    default:
                        errorGeneral = Textos.fallo(f)
                    }
                }
            } catch {
                errorGeneral = Textos.fallo(error)
            }
        }
    }
}

private struct Candidato: View {
    let c: WireCandidate
    let elegido: Bool
    let elegir: () -> Void

    var body: some View {
        Button(action: elegir) {
            HStack(spacing: 12) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(c.name).font(Tipo.texto(15, peso: 600)).foregroundStyle(Color.hueso)
                    Text(detalle).meta().lineLimit(1)
                }
                Spacer(minLength: 8)
                Image(systemName: elegido ? "checkmark.circle.fill" : "circle")
                    .font(.system(size: 16))
                    .foregroundStyle(elegido ? Color.hueso : Color.huesoTenue)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 14)
            .frame(minHeight: 56)
            .background(RoundedRectangle(cornerRadius: Medida.radioNodo).fill(Color.piedra))
            .overlay {
                RoundedRectangle(cornerRadius: Medida.radioNodo)
                    .strokeBorder(elegido ? Color.hueso : Color.veta, lineWidth: elegido ? 1.5 : 1)
            }
            .contentShape(RoundedRectangle(cornerRadius: Medida.radioNodo))
        }
        .buttonStyle(.plain)
        .accessibilityLabel("\(c.name), \(detalle)")
        .accessibilityAddTraits(elegido ? .isSelected : [])
    }

    /// The local candidate is «en este Mac», never presented as verified (any local user could
    /// listen on 127.0.0.1:8004).
    private var detalle: String {
        let local = c.address.contains("127.0.0.1") || c.address.contains("localhost")
        return local ? "en este Mac · versión \(c.version)" : "\(sinEsquema(c.address)) · versión \(c.version)"
    }
}

private struct YaEmparejado: View {
    let status: WireStatus
    @Environment(Navegacion.self) private var nav

    var body: some View {
        VStack(alignment: .leading, spacing: 24) {
            VStack(alignment: .leading, spacing: 12) {
                Text("Este Mac ya está emparejado.").titularFogar()
                    .fixedSize(horizontal: false, vertical: true)
                Text(status.gateway.map { "Está unido a \($0.name) (\($0.address)). Para emparejarlo con otro gateway, desemparéjalo primero en Ajustes." }
                     ?? "Para emparejarlo con otro gateway, desemparéjalo primero en Ajustes.")
                    .font(Tipo.cuerpo).suave()
                    .fixedSize(horizontal: false, vertical: true)
            }
            HStack(spacing: 12) {
                Button("Ver el estado") { nav.seccion = .estado }
                    .buttonStyle(Pildora(variante: .principal))
                Button("Ir a Ajustes") { nav.seccion = .ajustes }
                    .buttonStyle(Pildora())
            }
        }
    }
}

private struct SinAgente: View {
    let state: AgentState
    @Environment(AgentLink.self) private var link

    var body: some View {
        let t = Textos.titular(state)
        VStack(alignment: .leading, spacing: 24) {
            VStack(alignment: .leading, spacing: 12) {
                Text("Primero, el agente.").titularFogar()
                Text("Para emparejar hace falta el agente de Fogar en este Mac. \(t.sub)")
                    .font(Tipo.cuerpo).suave()
                    .fixedSize(horizontal: false, vertical: true)
                Estado(tono: t.tono, texto: t.titulo).font(Tipo.texto(14, peso: 550)).foregroundStyle(Color.hueso)
            }
            Button("Volver a llamar") { link.retryNow() }
                .buttonStyle(Pildora())
        }
    }
}

/// Addresses as the person writes them (the hint says «sin http://»).
func sinEsquema(_ address: String) -> String {
    address.replacingOccurrences(of: "http://", with: "")
}

/// macOS 26 draws a soft scroll-edge band under the title bar; on this full-bleed screen it reads
/// as a seam across the living background.
private struct SinBordeDeScroll: ViewModifier {
    func body(content: Content) -> some View {
        if #available(macOS 26, *) { content.scrollEdgeEffectHidden(true, for: .top) } else { content }
    }
}
