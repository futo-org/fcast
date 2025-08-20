package org.fcast.sdk.example.views

import android.annotation.SuppressLint
import android.content.Context
import android.os.Bundle
import android.view.LayoutInflater
import android.view.Menu
import android.view.MenuItem
import android.view.View
import android.view.ViewGroup
import android.view.WindowManager
import android.widget.AdapterView
import android.widget.ArrayAdapter
import android.widget.Button
import android.widget.EditText
import android.widget.ImageButton
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.ProgressBar
import android.widget.Spinner
import android.widget.TextView
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.app.AppCompatActivity
import androidx.constraintlayout.widget.ConstraintLayout
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.recyclerview.widget.LinearLayoutManager
import androidx.recyclerview.widget.RecyclerView
import androidx.recyclerview.widget.RecyclerView.ViewHolder
import com.google.android.material.slider.Slider
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import org.fcast.sender_sdk.DeviceConnectionState
import org.fcast.sender_sdk.ProtocolType
import org.fcast.sender_sdk.CastingDevice
import org.fcast.sender_sdk.DeviceEventHandler
import org.fcast.sender_sdk.IpAddr
import org.fcast.sender_sdk.PlaybackState
import org.fcast.sender_sdk.Source
import org.fcast.sender_sdk.GenericKeyEvent
import org.fcast.sender_sdk.GenericMediaEvent
import org.fcast.sender_sdk.initLogger
import org.fcast.sender_sdk.DeviceInfo
import org.fcast.sender_sdk.DeviceDiscovererEventHandler
import org.fcast.sender_sdk.CastContext
import org.fcast.sender_sdk.deviceInfoFromUrl
import org.fcast.sender_sdk.urlFormatIpAddr
import org.fcast.sender_sdk.LogLevelFilter
import org.fcast.sender_sdk.NsdDeviceDiscoverer
import org.fcast.sender_sdk.tryIpAddrFromStr

data class CastingState(
    var activeDevice: CastingDevice? = null,
    var volume: Double = 1.0,
    var playbackState: PlaybackState = PlaybackState.IDLE,
    var time: Double = 0.0,
    var duration: Double = 0.0,
    var speed: Double = 1.0,
    var contentType: String = "",
    var localAddress: IpAddr? = null,
) {
    fun reset() {
        volume = 1.0
        playbackState = PlaybackState.IDLE
        time = 0.0
        duration = 0.0
        speed = 1.0
        contentType = ""
        localAddress = null
    }
}

class EventHandler(
    private val castingState: CastingState,
    private val onConnected: () -> Unit,
    private val onVolumeChanged: (Double) -> Unit,
    private val onDurationChanged: (Double) -> Unit,
    private val onPositionChanged: (Double) -> Unit,
) :
    DeviceEventHandler {
    override fun connectionStateChanged(state: DeviceConnectionState) {
        println("Connection state changed: $state")
        when (state) {
            is DeviceConnectionState.Connected -> {
                castingState.localAddress = state.localAddr
                onConnected()
            }

            else -> {}
        }
    }

    override fun volumeChanged(volume: Double) {
        println("Volume changed: $volume")
        castingState.volume = volume
        onVolumeChanged(volume)
    }

    override fun timeChanged(time: Double) {
        println("Time changed: $time")
        castingState.time = time
        onPositionChanged(time)
    }

    override fun playbackStateChanged(state: PlaybackState) {
        println("Playback state changed: $state")
        castingState.playbackState = state
    }

    override fun durationChanged(duration: Double) {
        println("Duration changed: $duration")
        castingState.duration = duration
        onDurationChanged(duration)
    }

    override fun speedChanged(speed: Double) {
        println("Speed changed: $speed")
        castingState.speed = speed
    }

    override fun sourceChanged(source: Source) {
        println("Source changed: $source")
        when (source) {
            is Source.Url -> {
                castingState.contentType = source.contentType
            }

            else -> {
                castingState.contentType = ""
            }
        }
    }

    override fun keyEvent(event: GenericKeyEvent) {
        // Unreachable
    }

    override fun mediaEvent(event: GenericMediaEvent) {
        // Unreachable
    }

    override fun playbackError(message: String) {
        println("Playback error: $message")
    }
}

class DiscoveryEventHandler(
    private val onDeviceAdded: (DeviceInfo) -> Unit,
    private val onDeviceRemoved: (String) -> Unit,
    private val onDeviceUpdated: (DeviceInfo) -> Unit,
) : DeviceDiscovererEventHandler {
    override fun deviceAvailable(deviceInfo: DeviceInfo) {
        onDeviceAdded(deviceInfo)
    }

    override fun deviceChanged(deviceInfo: DeviceInfo) {
        onDeviceUpdated(deviceInfo)
    }

    override fun deviceRemoved(deviceName: String) {
        onDeviceRemoved(deviceName)
    }
}

class DeviceViewHolder(view: View, private val onConnect: (CastingDevice) -> Unit) :
    ViewHolder(view) {
    private val root: ConstraintLayout = view.findViewById(org.fcast.sender_sdk.R.id.layout_root)
    private val textName: TextView = view.findViewById(org.fcast.sender_sdk.R.id.text_name)
    private val imageDevice: ImageView = view.findViewById(org.fcast.sender_sdk.R.id.image_device)
    private val progressBar: ProgressBar = view.findViewById(org.fcast.sender_sdk.R.id.image_loader)
    private val textType: TextView = view.findViewById(org.fcast.sender_sdk.R.id.text_type)
    private var device: CastingDevice? = null

    init {
        root.setOnClickListener {
            device?.let {
                if (it.isReady()) {
                    onConnect(it)
                }
            }
        }
    }

    @SuppressLint("SetTextI18n")
    fun bind(d: CastingDevice) {
        when (d.castingProtocol()) {
            ProtocolType.CHROMECAST -> {
                imageDevice.setImageResource(org.fcast.sender_sdk.R.drawable.ic_chromecast)
                textType.text = "Chromecast"
            }

            ProtocolType.F_CAST -> {
                imageDevice.setImageResource(org.fcast.sender_sdk.R.drawable.ic_fc)
                textType.text = "FCast"
            }
        }

        textName.text = d.name()

        if (d.isReady()) {
            progressBar.visibility = View.GONE
        } else {
            progressBar.visibility = View.VISIBLE
        }

        device = d
    }
}

class DeviceAdapter(
    private val devices: List<CastingDevice>,
    private val onConnect: (CastingDevice) -> Unit
) : RecyclerView.Adapter<DeviceViewHolder>() {
    override fun onCreateViewHolder(parent: ViewGroup, viewType: Int): DeviceViewHolder {
        val view = LayoutInflater.from(parent.context)
            .inflate(org.fcast.sender_sdk.R.layout.list_device, parent, false)
        return DeviceViewHolder(view, onConnect)
    }

    override fun getItemCount(): Int {
        return devices.size
    }

    override fun onBindViewHolder(holder: DeviceViewHolder, position: Int) {
        holder.bind(devices[position])
    }
}

class ConnectCastingDialog(
    context: Context,
    private val onBarcode: () -> Unit,
    private val onConnect: (CastingDevice) -> Unit,
    private val onAddManually: () -> Unit,
) : AlertDialog(context) {
    val devices: MutableList<CastingDevice> = mutableListOf()
    private lateinit var adapter: DeviceAdapter
    private lateinit var recyclerDevices: RecyclerView
    private lateinit var textNoDevicesFound: TextView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(
            LayoutInflater.from(context)
                .inflate(org.fcast.sender_sdk.R.layout.dialog_casting_connect, null)
        )

        recyclerDevices = findViewById(org.fcast.sender_sdk.R.id.recycler_devices)!!
        textNoDevicesFound = findViewById(org.fcast.sender_sdk.R.id.text_no_devices_found)!!

        adapter = DeviceAdapter(devices, onConnect)
        recyclerDevices.adapter = adapter
        recyclerDevices.layoutManager = LinearLayoutManager(context)

        val buttonScanQr = findViewById<LinearLayout>(org.fcast.sender_sdk.R.id.button_qr)
        buttonScanQr?.setOnClickListener {
            onBarcode()
        }

        findViewById<Button>(org.fcast.sender_sdk.R.id.button_close)
            ?.setOnClickListener {
                this.hide()
            }
        findViewById<LinearLayout>(org.fcast.sender_sdk.R.id.button_add)
            ?.setOnClickListener {
                onAddManually()
            }
    }

    override fun show() {
        super.show()
        textNoDevicesFound.visibility = if (devices.isEmpty()) View.VISIBLE else View.GONE
        recyclerDevices.visibility = if (devices.isNotEmpty()) View.VISIBLE else View.GONE
    }

    fun update() {
        try {
            adapter.notifyDataSetChanged()
        } catch (e: Exception) {
            println("ConnectCastingDialog update failed: $e")
        }
    }
}

class DeviceConnectingDialog(context: Context) : AlertDialog(context) {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(
            LayoutInflater.from(context)
                .inflate(org.fcast.sender_sdk.R.layout.dialog_connecting_to_device, null)
        )
    }
}

class DeviceConnectedDialog(
    context: Context,
    private val castingState: CastingState,
    private val onDisconnected: () -> Unit,
) : AlertDialog(context) {
    private lateinit var imageDevice: ImageView
    private lateinit var textName: TextView
    private lateinit var textType: TextView
    lateinit var volumeSlider: Slider
    lateinit var positionSlider: Slider

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(
            LayoutInflater.from(context)
                .inflate(org.fcast.sender_sdk.R.layout.dialog_casting_connected, null)
        )

        imageDevice = findViewById(org.fcast.sender_sdk.R.id.image_device)!!
        textName = findViewById(org.fcast.sender_sdk.R.id.text_name)!!
        textType = findViewById(org.fcast.sender_sdk.R.id.text_type)!!
        findViewById<Button>(org.fcast.sender_sdk.R.id.button_close)
            ?.setOnClickListener {
                this.hide()
            }
        findViewById<Button>(org.fcast.sender_sdk.R.id.button_disconnect)
            ?.setOnClickListener {
                try {
                    castingState.activeDevice?.disconnect()
                } catch (e: Exception) {
                    println(e)
                }
                castingState.activeDevice = null
                castingState.reset()
                this.hide()
                onDisconnected()
            }
        findViewById<ImageButton>(org.fcast.sender_sdk.R.id.button_play)
            ?.setOnClickListener {
                castingState.activeDevice?.resumePlayback()
            }
        findViewById<ImageButton>(org.fcast.sender_sdk.R.id.button_pause)
            ?.setOnClickListener {
                castingState.activeDevice?.pausePlayback()
            }
        findViewById<ImageButton>(org.fcast.sender_sdk.R.id.button_stop)
            ?.setOnClickListener {
                castingState.activeDevice?.stopPlayback()
            }
        volumeSlider = findViewById(org.fcast.sender_sdk.R.id.slider_volume)!!
        volumeSlider.addOnChangeListener(Slider.OnChangeListener { _, value, fromUser ->
            if (fromUser) {
                castingState.activeDevice?.changeVolume(value.toDouble())
            }
        })
        positionSlider = findViewById(org.fcast.sender_sdk.R.id.slider_position)!!
        positionSlider.addOnChangeListener(Slider.OnChangeListener { _, value, fromUser ->
            if (fromUser) {
                castingState.activeDevice?.seek(value.toDouble())
            }
        })
    }

    fun update() {
        val device = castingState.activeDevice ?: return
        when (device.castingProtocol()) {
            ProtocolType.CHROMECAST -> {
                imageDevice.setImageResource(org.fcast.sender_sdk.R.drawable.ic_chromecast)
                textType.text = "Chromecast"
            }

            ProtocolType.F_CAST -> {
                imageDevice.setImageResource(org.fcast.sender_sdk.R.drawable.ic_fc)
                textType.text = "FCast"
            }
        }
        textName.text = device.name()
    }
}

class CastingAddDialog(context: Context, val onAdded: (DeviceInfo) -> Unit) : AlertDialog(context) {
    private lateinit var textError: TextView
    private lateinit var editName: EditText
    private lateinit var editIP: EditText
    private lateinit var editPort: EditText
    private lateinit var spinnerType: Spinner

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(
            LayoutInflater.from(context)
                .inflate(org.fcast.sender_sdk.R.layout.dialog_casting_add, null)
        )

        findViewById<Button>(org.fcast.sender_sdk.R.id.button_cancel)
            ?.setOnClickListener {
                this.hide()
            }

        textError = findViewById(org.fcast.sender_sdk.R.id.text_error)!!
        textError.visibility = View.GONE
        editName = findViewById(org.fcast.sender_sdk.R.id.edit_name)!!
        editIP = findViewById(org.fcast.sender_sdk.R.id.edit_ip)!!
        editPort = findViewById(org.fcast.sender_sdk.R.id.edit_port)!!
        spinnerType = findViewById(org.fcast.sender_sdk.R.id.spinner_type)!!

        ArrayAdapter.createFromResource(
            context,
            org.fcast.sender_sdk.R.array.casting_device_type_array,
            org.fcast.sender_sdk.R.layout.spinner_item_simple
        ).also { adapter ->
            adapter.setDropDownViewResource(org.fcast.sender_sdk.R.layout.spinner_dropdownitem_simple)
            spinnerType.adapter = adapter
        }

        spinnerType.onItemSelectedListener = object : AdapterView.OnItemSelectedListener {
            override fun onItemSelected(p0: AdapterView<*>?, p1: View?, p2: Int, p3: Long) {
                editPort.text?.clear()
                editPort.text?.append(
                    when (spinnerType.selectedItemPosition) {
                        0 -> "46899" // FCast
                        1 -> "8009" // Chromecast
                        else -> ""
                    }
                )
            }

            override fun onNothingSelected(p0: AdapterView<*>?) = Unit
        }

        findViewById<Button>(org.fcast.sender_sdk.R.id.button_confirm)
            ?.setOnClickListener {
                val castProtocolType = when (spinnerType.selectedItemPosition) {
                    0 -> ProtocolType.F_CAST
                    1 -> ProtocolType.CHROMECAST
                    else -> {
                        textError.text =
                            "Device type is invalid expected values like FastCast or ChromeCast."
                        textError.visibility = View.VISIBLE
                        return@setOnClickListener
                    }
                }

                val name = editName.text.toString().trim()
                if (name.isBlank()) {
                    textError.text = "Name can not be empty."
                    textError.visibility = View.VISIBLE
                    return@setOnClickListener
                }

                val ip = editIP.text.toString().trim()
                if (ip.isBlank()) {
                    textError.text = "IP can not be empty."
                    textError.visibility = View.VISIBLE
                    return@setOnClickListener
                }

                val address = try {
                    tryIpAddrFromStr(ip)
                } catch (e: Exception) {
                    println("Invalid IP address ($ip): $e")
                    textError.text = "IP address is invalid"
                    textError.visibility = View.VISIBLE
                    return@setOnClickListener
                }
                val port: UShort? = editPort.text.toString().trim().toUShortOrNull();
                if (port == null) {
                    textError.text = "Port number is invalid, expected a number between 0 and 65535.";
                    textError.visibility = View.VISIBLE;
                    return@setOnClickListener;
                }

                textError.visibility = View.GONE;
                val deviceInfo = DeviceInfo(name, castProtocolType, listOf(address), port);
                onAdded(deviceInfo)
                dismiss()
            }
    }

    override fun show() {
        super.show()

        editName.text.clear()
        editIP.text.clear()
        editPort.text.clear()
        editPort.text.append("46899")
        textError.visibility = View.GONE
        spinnerType.setSelection(0)

        window?.apply {
            clearFlags(WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE)
            clearFlags(WindowManager.LayoutParams.FLAG_ALT_FOCUSABLE_IM)
            setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_STATE_ALWAYS_VISIBLE)
        }
    }
}

class MainActivity : AppCompatActivity() {
    private val castingState = CastingState()
    private val eventHandler = EventHandler(
        castingState,
        {
            CoroutineScope(Dispatchers.Main).launch {
                connectingToDeviceDialog.hide()
                castingConnectedDialog.show()
                castingConnectedDialog.update()
                castLocalFileBtn.visibility = View.VISIBLE
            }
        },
        { newVolume ->
            CoroutineScope(Dispatchers.Main).launch {
                try {
                    castingConnectedDialog.volumeSlider.value = newVolume.toFloat()
                        .coerceAtMost(castingConnectedDialog.volumeSlider.valueTo)
                } catch (e: Exception) {
                    println("$e")
                }
            }
        },
        { newDuration ->
            CoroutineScope(Dispatchers.Main).launch {
                try {
                    val newDurationF = newDuration.toFloat()
                    castingConnectedDialog.positionSlider.value =
                        castingConnectedDialog.positionSlider.value.coerceAtMost(newDurationF)
                    castingConnectedDialog.positionSlider.valueTo = newDurationF
                } catch (e: Exception) {
                    println("$e")
                }
            }
        },
        { newPosition ->
            CoroutineScope(Dispatchers.Main).launch {
                try {
                    val newPositionF = newPosition.toFloat()
                    castingConnectedDialog.positionSlider.value = newPositionF
                    castingConnectedDialog.positionSlider.value =
                        castingConnectedDialog.positionSlider.valueTo.coerceAtMost(newPositionF)
                } catch (e: Exception) {
                    println("$e")
                }
            }
        })
    private val castContext = CastContext()
    private val fileServer = castContext.startFileServer()
    private lateinit var connectCastingDialog: ConnectCastingDialog
    private lateinit var castingConnectedDialog: DeviceConnectedDialog
    private lateinit var castingAddDialog: CastingAddDialog
    private lateinit var connectingToDeviceDialog: DeviceConnectingDialog
    private val barcodeLauncher = registerForActivityResult(ScanContract()) { result ->
        result.contents?.let {
            deviceInfoFromUrl(it)?.let { deviceInfo ->
                val device = castContext.createDeviceFromInfo(deviceInfo)
                try {
                    castingState.reset()
                    device.connect(null, eventHandler)
                    castingState.activeDevice = device
                } catch (e: Exception) {
                    println("Failed to start device: {e}")
                }
            }
        }
    }
    private val selectMediaIntent = registerForActivityResult(ActivityResultContracts.GetContent())
    { maybeUri ->
        try {
            val uri = maybeUri!!
            val type = this.contentResolver.getType(uri)!!
            val parcelFd = this.contentResolver.openFileDescriptor(uri, "r")
            val fd = parcelFd?.detachFd() ?: throw Exception("asdf")
            castingState.activeDevice?.let { device ->
                val entry = fileServer.serveFile(fd)
                val url =
                    "http://${urlFormatIpAddr(castingState.localAddress!!)}:${entry.port}/${entry.location}"
                device.loadUrl(type, url, null, null, null, null, null)
            }
        } catch (e: Exception) {
            println("Failed to read $maybeUri: $e")
        }
    }
    private lateinit var deviceDiscoverer: NsdDeviceDiscoverer
    private lateinit var castLocalFileBtn: Button

    init {
        initLogger(LogLevelFilter.DEBUG)
    }

    override fun onCreateOptionsMenu(menu: Menu?): Boolean {
        super.onCreateOptionsMenu(menu)
        menuInflater.inflate(R.menu.actions, menu)
        return true
    }

    override fun onOptionsItemSelected(item: MenuItem): Boolean {
        when (item.itemId) {
            R.id.cast_button -> {
                if (castingState.activeDevice != null) {
                    castingConnectedDialog.show()
                    castingConnectedDialog.update()
                } else {
                    connectCastingDialog.show()
                    connectCastingDialog.update()
                }
                return true
            }

            else -> {
                return super.onOptionsItemSelected(item)
            }
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        deviceDiscoverer = NsdDeviceDiscoverer(
            this, DiscoveryEventHandler(
                { deviceInfo ->
                    CoroutineScope(Dispatchers.Main).launch {
                        try {
                            connectCastingDialog.devices.add(
                                castContext.createDeviceFromInfo(
                                    deviceInfo
                                )
                            )
                            connectCastingDialog.update()
                        } catch (e: Exception) {
                            println(e)
                        }
                    }
                },
                { deviceName ->
                    CoroutineScope(Dispatchers.Main).launch {
                        try {
                            connectCastingDialog.devices.removeIf { it.name() == deviceName }
                            connectCastingDialog.update()
                        } catch (e: Exception) {
                            println(e)
                        }
                    }
                },
                { deviceInfo ->
                    CoroutineScope(Dispatchers.Main).launch {
                        try {
                            connectCastingDialog.devices.find { it.name() == deviceInfo.name }
                                ?.let { device ->
                                    device.setPort(deviceInfo.port)
                                    device.setAddresses(deviceInfo.addresses)
                                }
                        } catch (e: Exception) {
                            println(e)
                        }
                    }
                })
        )
        enableEdgeToEdge()
        connectCastingDialog = ConnectCastingDialog(
            this,
            {
                barcodeLauncher.launch(ScanOptions().setOrientationLocked(false))
            },
            { device ->
                connectCastingDialog.hide()
                try {
                    device.connect(null, eventHandler)
                    castingState.activeDevice = device
                    connectingToDeviceDialog.show()
                } catch (e: Exception) {
                    println(e)
                }
            },
            {
                connectCastingDialog.hide()
                castingAddDialog.show()
            })
        castingConnectedDialog = DeviceConnectedDialog(this, castingState) {
            castLocalFileBtn.visibility = View.GONE
        }
        castingAddDialog = CastingAddDialog(this) { deviceInfo ->
            try {
                connectCastingDialog.devices.add(
                    castContext.createDeviceFromInfo(
                        deviceInfo
                    )
                )
                connectCastingDialog.update()
            } catch (e: Exception) {
                println(e)
            }
        }
        connectingToDeviceDialog = DeviceConnectingDialog(this)
        setContentView(R.layout.activity_main)
        setSupportActionBar(findViewById(R.id.toolbar))
        supportActionBar?.setDisplayShowTitleEnabled(false)

        castLocalFileBtn = findViewById(R.id.cast_local_file)
        castLocalFileBtn.visibility = View.GONE
        castLocalFileBtn.setOnClickListener {
            selectMediaIntent.launch("*/*")
        }

        ViewCompat.setOnApplyWindowInsetsListener(findViewById(R.id.main)) { v, insets ->
            val systemBars = insets.getInsets(WindowInsetsCompat.Type.systemBars())
            v.setPadding(systemBars.left, systemBars.top, systemBars.right, systemBars.bottom)
            insets
        }
    }
}