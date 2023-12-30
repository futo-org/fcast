import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import org.bouncycastle.asn1.x500.X500Name
import org.bouncycastle.asn1.x509.SubjectPublicKeyInfo
import org.bouncycastle.cert.X509v3CertificateBuilder
import org.bouncycastle.cert.jcajce.JcaX509CertificateConverter
import org.bouncycastle.jce.provider.BouncyCastleProvider
import org.bouncycastle.operator.jcajce.JcaContentSignerBuilder
import java.io.FileInputStream
import java.math.BigInteger
import java.security.KeyPairGenerator
import java.security.KeyStore
import java.security.PrivateKey
import java.security.PublicKey
import java.util.Calendar
import javax.net.ssl.KeyManagerFactory
import javax.net.ssl.SSLContext
import javax.net.ssl.SSLServerSocketFactory
import java.security.cert.X509Certificate
import javax.net.ssl.TrustManagerFactory

class SslKeyManager(private val alias: String) {

    fun getSslServerSocketFactory(): SSLServerSocketFactory {
        val keyStore = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        //if (!keyStore.containsAlias(alias)) {
            generateKeyPairAndCertificate(keyStore)
        //}

        val trustManagerFactory = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm()).apply {
            init(keyStore)
        }

        val keyManagerFactory = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm()).apply {
            init(keyStore, null)
        }

        val sslContext = SSLContext.getInstance("TLS").apply {
            init(keyManagerFactory.keyManagers, trustManagerFactory.trustManagers, null)
        }

        return sslContext.serverSocketFactory
    }

    private fun generateKeyPairAndCertificate(keyStore: KeyStore) {
        val keyPairGenerator = KeyPairGenerator.getInstance("RSA", "AndroidKeyStore")
        val parameterSpec = KeyGenParameterSpec
            .Builder(alias, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT or KeyProperties.PURPOSE_SIGN or KeyProperties.PURPOSE_VERIFY)
            //.setBlockModes(KeyProperties.BLOCK_MODE_ECB)
            .setDigests(KeyProperties.DIGEST_SHA256, KeyProperties.DIGEST_SHA512)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_RSA_PKCS1)
            .setSignaturePaddings(KeyProperties.SIGNATURE_PADDING_RSA_PKCS1)
            .build()

        keyPairGenerator.initialize(parameterSpec)

        val keyPair = keyPairGenerator.generateKeyPair()
        val privateKey = keyPair.private
        val publicKey = keyPair.public
        val cert = generateSelfSignedCertificate(privateKey, publicKey)
        keyStore.setKeyEntry(alias, privateKey, null, arrayOf(cert))
    }

    private fun generateSelfSignedCertificate(privateKey: PrivateKey, publicKey: PublicKey): X509Certificate {
        val start = Calendar.getInstance().time
        val end = Calendar.getInstance().apply { add(Calendar.YEAR, 1000) }.time

        val certInfo = X509v3CertificateBuilder(
            X500Name("CN=FCastReceiver"),
            BigInteger.ONE,
            start,
            end,
            X500Name("CN=FCastReceiver"),
            SubjectPublicKeyInfo.getInstance(publicKey.encoded)
        )

        val signer = JcaContentSignerBuilder("SHA256withRSA").build(privateKey)
        return JcaX509CertificateConverter().getCertificate(certInfo.build(signer))
    }
}